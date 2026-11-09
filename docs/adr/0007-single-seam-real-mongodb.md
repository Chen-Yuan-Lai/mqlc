# 測試只有一個主要 seam，只假造 LLM，MongoDB 用真的

所有行為（knowledge 組裝、static check、repair、value correction、context overflow）都透過 `mqlc-core` 的單題 API 測試。唯一的 test double 是腳本化的 `LlmClient`，依序回傳寫好的回覆；MongoDB 一律用 testcontainers 起真的 `mongo:8.2`，不 mock。mqlc 的價值幾乎都在「pipeline 在真的 MongoDB 上會怎樣」：假的資料庫驗證不了 `$unwind` 語意、型別比較或 Extended JSON 日期。代價是測試需要 Docker、比較慢。例外只有三個：IR round-trip（不需資料庫）、`mqlc bench` 續跑（在 CLI 層測）、MCP smoke test。
