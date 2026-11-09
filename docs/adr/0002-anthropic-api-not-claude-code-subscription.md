# Benchmark 用 Anthropic API key，不用 Claude Code 訂閱

原本打算用 Pro 訂閱的額度（`claude -p`）跑 benchmark 來省錢，實測後放棄：即使指定 `--system-prompt` 和 `--tools ""`，每次呼叫仍會帶入約 49k tokens 的使用者設定、plugin 與 hook context（`--bare` 又不支援訂閱登入），污染了官方 prompt，也無法設定 temperature，且有使用條款疑慮。因此 benchmark 一律走 Messages API（含 prompt caching 與 Message Batches），預算上限 USD 100、USD 80 時停下檢視。

`claude -p` 只用在實驗 E：那個實驗量的就是「宿主 LLM 加 MCP 工具」，Claude Code 本身是受測對象。
