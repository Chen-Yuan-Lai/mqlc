# 實驗 B 的兩組，與靜態檢查只報 schema 排除得了的錯誤

實驗 B 比較兩個修正組（#12），以 `--repair` 選擇，與 `--arm`（prompt 裡的知識）互相獨立：`execution`（對照組：執行報錯就把錯誤回饋給模型重生）與 `static-check`（IR 組：執行前先以 IR 對照官方說明做 static check，沒過就回饋重生，之後同樣有執行錯誤修正）。Manifest 以 `config.ir` 與 `config.max_repairs` 記錄是哪一組；兩者都不開時不寫 `max_repairs`，#12 之前的 manifest 照常續跑。下列做法會直接影響分數：

**修正次數分開計算，各 2 次。** IR 組最多 2 次 static check 修正、另有 2 次執行錯誤修正，所以一題最多呼叫 5 次，對照組最多 3 次。這照 parent spec 的寫法；共用上限會讓 IR 組留給執行錯誤的次數變少，兩組的執行修正就不再相同。代價是 IR 組可以多花錢，所以比較時要一併看修正 token 與花費（`score.py` 的 `repairs`）。每個 candidate 只要 static check 還有次數就會檢查，包括執行修正後的新 candidate。

**修正 2 次後仍沒通過 static check 的 candidate 照樣執行、照常計分。** 官方說明本身有錯（#20 找到 120 個資料裡不存在的官方欄位），static check 的錯誤可能是誤報；記成答錯會讓誤報直接壓低分數。仍找得到的錯誤記在紀錄的 `check_errors`。

**Static check 只報 schema 排除得了的錯誤。** 檢查從起始 collection 的官方欄位出發，逐個 stage 推算下一個 stage 看到的欄位與型別（`$project`、`$group`、`$lookup`、`$unwind`、`$addFields` 等都會改變欄位）。描述成 `Document` 或 `Array` 卻沒列出底下欄位的欄位、型別不明的值，以及 raw stage 之後的一切，都視為可能有任何欄位、任何型別，不檢查。型別只分到 `String`、`Number`（所有數值型別）、`Date`、`Boolean`、`ObjectId`、`Document`、`Array`、`Null` 等：比較時字面值的型別不在欄位允許的型別裡才報（`null` 不報；陣列欄位與元素比較，元素型別不明，不報）；`$year`、`$dateDiff` 的欄位必須可能是 `Date`。官方說明在欄位名稱前加 collection 名稱的（`schools.cdsCode`），只要該 collection 沒有同名欄位，就去掉前綴（同 #20）。在 train 的 1,762 條 gold pipeline 上（`mqlc static-check`），檢查什麼都沒報；把每條 gold pipeline 第一個 `$match` 的欄位名改錯，1,503 條全部抓到（一次性的測量，程式不在 repo 裡）。這只是 train 的數字：調整檢查時只看 train（ADR 0006），`static-check` 只接受 train。Gold pipeline 都只用 IR 認得的 operator；模型的回覆用到其他 operator（例如 `$regex`、`$round`）時，那個 stage 是 raw stage，之後不檢查，所以這個召回率是上限。只說 `Null` 的官方型別（6 個欄位）是轉換時沒看到值，視為型別不明；只做排除的 `$project` 不檢查欄位，MongoDB 排除不存在的欄位不會出錯。`$unwind` 用在非陣列上在 MongoDB 不會出錯（當成只有一個元素），照 spec 仍當成錯誤回報。`static-check` 的報告由 core 算好、CLI 印出，沒有自動化測試（ADR 0007 的 CLI 層）。

**修正是多輪對話，一律同步呼叫。** 修正請求是原本的 prompt，接著模型的回覆與我們的回饋（`LlmRequest::turns`），所以 schema 區塊的 prompt cache 照樣命中；回饋文字是我們自訂的（「The pipeline in your reply does not fit the MongoDB schema description:」或「Running the pipeline in your reply failed:」，列出錯誤，請模型用同樣格式重寫整個答案）。Batch 模式下第一次回覆照常走 batch、以半價計，修正則在收回結果時同步呼叫、以全價計；紀錄的 `usage` 與 `cost_usd` 含所有呼叫，`repairs` 逐次記錄錯誤、回覆、token、花費。最後一次回覆就是答案：修正後的回覆被截斷或無法解析，就照 ADR 0011 記成該題的錯誤。修正呼叫的 API 失敗屬於基礎設施，run 停下、這題不寫紀錄；batch 模式下這批結果不標為已收回，重跑時從 batch 結果重新修正，修正可能付兩次錢。多輪對話下若 prompt 估算已放不進 context window，就不再修正，目前的 candidate 照常執行。

IR 組送出的知識多了 static check 用的官方說明，所以它的 manifest 知識 hash 也包含官方說明：官方說明改了就拒絕續跑，即使 prompt 用的是 MDL。
