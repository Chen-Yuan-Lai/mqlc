# 所有邏輯放在 mqlc-core，Python 只負責計分

CLI（`mqlc`）和 MCP server（`mqlc-mcp`）都只是 `mqlc-core` 的薄入口，新功能只在 core 實作一次。評測端雖然官方腳本是 Python，但 Python harness 只讀 run 的結果並呼叫 `eval_script.py` 計分，不呼叫 LLM、也不組 prompt。這樣 benchmark 量到的就是使用者實際會用的同一套程式，而不是另一份 Python 重寫版本。
