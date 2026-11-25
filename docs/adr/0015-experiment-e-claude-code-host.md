# 實驗 E：Claude Code 當宿主，兩組只差 MCP server

實驗 E 比較宿主 LLM 拿到的 MCP 工具（#16）：`mcp-official`（官方 MongoDB MCP server）與 `mcp-official-mqlc`（官方 MCP server 加 `mqlc-mcp`）。受測對象是 Claude Code 本身（ADR 0002），所以 `--backend claude-code` 每題跑一次 `claude -p`，最後的回覆就是 candidate，和其他 run 一樣由 mqlc 解析（ADR 0010）、在 MongoDB 上執行、以 `score.py` 計分。下列做法會直接影響分數：

**兩組的 prompt 相同，而且沒有 schema。** 量的是工具組合，所以資料庫的結構只能從工具取得。官方 prompt 的 system prompt 原文以 `--append-system-prompt` 接在 Claude Code 自己的 system prompt 之後（輸出格式照舊，回覆照樣能解析）；user turn 是下面這句話，空一行後接官方的問題區塊，不送 evidence：

> The MongoDB Schema Description is not given: look up the collections and fields of the database `<db_id>` with your tools.

Manifest 的 `config.knowledge` 記的是每個資料庫的這句話。

**兩組只差 MCP server，其他都相同。** 官方 server 是 `mongodb-mcp-server` 3.4.0（版本鎖在 `tools/mongodb-mcp/package-lock.json`，以 `scripts/install-mcp.sh` 安裝），以 `--readOnly` 和 `MDB_MCP_TELEMETRY=disabled` 啟動，它的工具全部可用，包括 `aggregate`、`find`、`count`，所以宿主可以先跑查詢再回答；這是真實的使用方式。第二組再加上 `mqlc-mcp`（`profile_schema`、`validate_pipeline`，工具描述見 ADR 0008）。兩個 server 用同一個唯讀的連線字串（`--mongo-uri`）。Claude Code 的內建工具全部關掉（`--tools ""`），`--allowedTools` 只放這一組的 server，`--strict-mcp-config` 不讀其他的 MCP 設定。

**Claude Code 與使用者的設定隔離。** `--setting-sources ""` 不讀 user、project、local 設定，所以使用者的 plugin、hook、權限都不會載入（ADR 0002 量到的 49k token 額外 context，實測降到約 6.8k）；`claude` 在一個空的暫存目錄（`$TMPDIR/mqlc-claude-code`）裡跑，讀不到 repo 的 CLAUDE.md；另加 `--disable-slash-commands` 與 `--no-session-persistence`。登入沿用 Claude Code 自己的：子行程的環境拿掉 `ANTHROPIC_API_KEY` 與 `ANTHROPIC_AUTH_TOKEN`，否則 `mqlc` 從 `.env` 讀進來的 key 會讓 Claude Code 改用 API 計費；`--bare` 不支援訂閱登入，所以不用。

**模型是 Haiku 4.5，取樣設定由 Claude Code 決定。** `--model claude-haiku-4-5`，和 baseline 與實驗 A、B 相同，分數可以對照。Claude Code 不接受 temperature 與 max_tokens，而且預設開啟 thinking，所以 manifest 裡的 `temperature`、`max_tokens`、`thinking` 在這個 backend 不代表實際設定；CLI 拒絕 `--temperature`、`--thinking-budget`、`--single-message`。宿主自己決定何時停止，最多 30 個 turn（`--max-turns 30`）。

**修正、值校正與 batch 都不開。** 宿主已經能執行查詢、看到錯誤後自行修正；再由 mqlc 回饋就是多一層 LLM 迴圈，也無法續用同一個 session。CLI 拒絕 `--repair`、`--value-correction`、`--mode batch`。

**失敗照 ADR 0011 分類。** 基礎設施失敗會讓 run 停下，這題不寫紀錄：MCP server 沒有連上（init 事件的 `mcp_servers` 有任何一個不是 `connected`，或這組設定的 server 不在其中；沒連上時模型會自己編出一段假的工具呼叫，記成答錯就是在量 server 的安裝問題；啟動較慢而回報 `pending` 也一樣停下，重跑即可）；Claude Code 回報錯誤（`is_error`，包括 API 錯誤、rate 與額度限制）；無法啟動 `claude`、送不出 prompt、建不了工作目錄；輸出有讀不懂的行；沒有輸出 result 就結束；15 分鐘仍未結束（`claude` 在自己的 process group 裡跑，逾時連同它啟動的 MCP server 一起結束，以免繼續佔用 MongoDB）。這些都不重試，因為重試要整個 session 重跑一次。用完 30 個 turn 仍沒有回覆是模型自己的結果，記為 `max_turns`，算答錯。

**花費是 Claude Code 自己估的。** 每筆紀錄的 `cost_usd` 是 result 事件的 `total_cost_usd`：Claude Code 依 list price 算出的所有呼叫的花費（它用 1 小時的 prompt cache，cache 寫入以 2 倍計），不是 `pricing.rs` 算的；用訂閱登入時不會照這個金額扣款，但它反映消耗的額度，`--spend-limit` 也照它計算。`usage` 是 result 事件裡加總所有 turn 的 token。`tool_calls` 記錄每個工具被呼叫幾次（以 Claude Code 的名稱，例如 `mcp__mongodb__find`）；`score.py` 在 `summary.json` 的 `tool_calls` 報告每題平均次數、沒呼叫任何工具的題數，以及每個工具的次數與用到它的題數。

**抽樣以 seed 固定，記在 manifest。** `mqlc bench --split dev --draw 300 --seed 16`（16 是 issue 編號）從 dev 的 1,424 題抽 300 題（`bench::Draw`）：對資料集順序的題目做 Fisher-Yates 洗牌，隨機數用 SplitMix64，自己寫而不用 crate，所以不會因為 crate 版本改變抽到的題目。Manifest 的 `config.draw` 記下題數與 seed，換 seed 續跑同一個 run 目錄會被拒絕（`bench::run` 測）；洗牌本身照 ADR 0007 篩選題目的規則，沒有另外的測試。

**測試 seam：假的 `claude` 執行檔。** ADR 0007 的第七個例外：`claude_code` 測試以 `bench::run` 跑 `ClaudeCodeClient`，`claude` 換成一支 shell script，記下收到的參數並印出寫好的 stream-json transcript，其餘（解析、執行、紀錄）照常對 testcontainers 上的 MongoDB 進行。測試檢查參數（system prompt、MCP 設定、隔離旗標）、子行程環境沒有 API key、紀錄的工具呼叫與花費、`max_turns` 記成答錯，以及上述基礎設施失敗都停下 run。真正的 `claude` 輸出格式只用真的 Claude Code 跑幾題確認，和 HTTP 後端的轉接層一樣。
