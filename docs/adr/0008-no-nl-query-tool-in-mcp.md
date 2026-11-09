# MCP server 不提供自然語言查詢工具

`mqlc-mcp` 只提供 `profile_schema` 和 `validate_pipeline`，不提供「輸入問題、回傳 pipeline」的工具。宿主（例如 Claude Code）本身就是 LLM，再包一層 mqlc 的 LLM 呼叫只會多花一次 token、多一層延遲，也讓宿主看不到中間的 knowledge。mqlc 在 MCP 情境的價值是 knowledge 和 static check，MQL 由宿主自己寫。
