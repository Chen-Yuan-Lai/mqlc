# 值校正檢查每個執行成功的 candidate，只在有相近的值時回饋

值校正（#13）以 `--value-correction` 開啟，與 `--arm`、`--repair` 互相獨立，預設關閉，manifest 以 `config.value_correction` 記錄；沒開時不寫這個 key，之前的 manifest 照常續跑，實驗 B 的兩組也不受影響。下列做法會直接影響分數：

**不分欄位基數，每個字串字面值都檢查。** Parent spec 寫的是「高基數欄位」，但官方說明沒有基數資訊，只有 auto MDL 的 `mqlc.distinct` 有；以它為準會讓 official arm 用不了，執行時另算 distinct 又讓每題多一個可能很慢的查詢。不存在的值不論基數都是不存在，低基數欄位的值域本來就在 prompt 裡，模型很少寫錯，多一次 `countDocuments`（`limit 1`）的成本很低。

**每個執行成功的 candidate 都檢查，不只結果為空的。** `$in: ["Jones, Bob", "Alice Smith"]` 只有一個值不存在時結果仍非空，只看空結果會漏掉。觸發條件只有「字面值不存在」：結果為空但字面值都存在時不校正。

**只檢查一定要相等的字面值。** 起始 collection 的文件還沒被改形狀之前（`$match`、`$sort`、`$limit`、`$skip`、`$unwind`、`$lookup`）的 `$match` 裡，欄位等於字串、`$eq`、`$in`、`$all`，含 `$and` 與 `$elemMatch` 裡的；`$or`、`$expr`、`$ne`、`$nin` 不檢查，那裡不存在的值可能正是題目要的。`$lookup`（`localField`/`foreignField`）之後，`as` 底下的路徑到 `from` collection 檢查；pipeline 形式的 `$lookup` 與 `includeArrayIndex` 加出的欄位不檢查；遇到其他 stage（含 raw stage）就停。

**候選值由窄到寬撈、Jaro-Winkler 排序，沒有相近的值就不校正。** 依序試三個不分大小寫的 regex，用第一個撈得到值的：含整個字面值、含字面值的每個字詞（順序不拘，所以 `Alice Smith` 撈得到 `Smith, Alice`）、含任一個三字元以上的字詞。每個 regex 依值分組、依值排序後最多讀 1,000 個不同的值：以值而不是文件計數，很多文件共有的值才不會把其他值擠掉；由窄到寬，`county` 這種很多值共有的字詞才不會把 `marin county` 擠出 1,000 個之外；排序讓每次撈到的值相同。取 `strsim` Jaro-Winkler 分數最高的 5 個值，分數取原字串（小寫）與字詞排序後兩者較高者。一個相近的值都沒有時不回饋：多半是題目本來就問資料裡沒有的值，或用錯欄位，候選值幫不上忙，回饋只會讓模型改壞對的答案。每個檢查查詢限時 10 秒。

回饋是「Values in the pipeline in your reply do not occur in the database:」，每個值一行（「no animals document has "Alice Smith" in owner.name; the closest values there are "Smith, Alice", ...」），接著「If the question means one of these values, use it and reply with the whole answer again, in the same format. If it means a value that is not in the data, reply with the same answer again.」模型回了同樣的 pipeline 就不再校正，不為同樣的回饋再付一次錢。

**校正最多 2 次，與修正分開計算，記在同一個 `repairs`。** 紀錄的 kind 是 `value_correction`，錯誤是 `missing_value`；呼叫方式、token 與花費的記法、batch 模式下同步以全價呼叫、context window 放不下時不再校正，都與修正相同（ADR 0013）。最後一次校正後的 candidate 照常計分，即使值仍不存在；它執行失敗時也一樣（沒開執行修正就記成答錯），即使校正前的 candidate 跑得通。檢查時 MongoDB 連不上屬於基礎設施（ADR 0011），run 停下；伺服器拒絕某個檢查查詢或查詢逾時，只略過那個字面值，pipeline 本身已經跑過了。
