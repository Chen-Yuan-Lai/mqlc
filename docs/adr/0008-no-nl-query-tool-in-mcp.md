# MCP server 不提供自然語言查詢工具

`mqlc-mcp` 只提供 `profile_schema` 和 `validate_pipeline`，不提供「輸入問題、回傳 pipeline」的工具。宿主（例如 Claude Code）本身就是 LLM，再包一層 mqlc 的 LLM 呼叫只會多花一次 token、多一層延遲，也讓宿主看不到中間的 knowledge。mqlc 在 MCP 情境的價值是 knowledge 和 static check，MQL 由宿主自己寫。

**Static check 對照 profile 出的 schema，不對照官方說明（#15）。** MCP 只拿到一個連線字串，任意資料庫都沒有官方說明，所以 `validate_pipeline` 對該資料庫跑 `profile::profile`（預設選項：每個 collection 讀 `_id` 順序的前 100,000 份文件、不排除任何欄位），以 `ir::Schema::from_mdl` 把 MDL 轉成 schema，再用 CLI `--repair static-check` 同一個 `Pipeline::check`，所以錯誤的種類與訊息和 CLI 相同。型別名稱照 `from_official` 的規則轉換（每種數值都是 `Number`；ADR 0013 對只說 `Null` 的官方型別的處理，同樣用在樣本裡只看過 null 的欄位：視為型別不明；`Regex` 不在轉換表裡，所以 regex 欄位也是型別不明）。陣列裡的文件照樣列出底下的欄位，所以和官方說明列出子欄位的 `Array` 一樣會檢查。代價是抽樣：樣本裡從沒出現的欄位會被報成不存在。每個資料庫第一次檢查時 profile 一次，之後沿用同一份，直到再呼叫 `profile_schema`；它重新 profile、取代這份 schema，並回傳 `MdlProject::render()` 的文字，也就是 `--arm auto-mdl` 放進 prompt 的同一份描述。沒有任何 collection 的資料庫（MongoDB 對不存在的資料庫也是如此）回傳工具錯誤「database `<name>` has no collections」，不回空的描述。`validate_pipeline` 回傳 `errors`（`kind`、`message`）與 `unchecked`（檢查讀不懂、之後不再檢查的 operator，同紀錄的 `unchecked`）。MCP 沒有 `--exclude`：profile 會把最常見的值交給宿主，README 提醒別把它接到宿主不該讀的資料上。

工具描述是宿主 LLM 讀的文字，實驗 E 的分數會受它影響，原文如下。`profile_schema`：

> Describe a MongoDB database for writing aggregation pipelines: each collection with its field paths (nested and array fields included), the BSON types seen, how often a field is missing, the `_id`s it refers to, and its values or their format, range and most common ones. Read from a sample of up to 100,000 documents per collection. Call it again after the data changes: validate_pipeline checks against the latest profile.

`validate_pipeline`：

> Check an aggregation pipeline against the database's profile before running it. Reports collections and field paths that do not exist at that stage, values compared with fields that never hold their type, and $unwind on fields that are never arrays. It reports only what the profile rules out: no errors does not make the pipeline right. `unchecked` lists the operators it cannot read; it checks nothing after them. A field missing from every sampled document is reported as missing.
