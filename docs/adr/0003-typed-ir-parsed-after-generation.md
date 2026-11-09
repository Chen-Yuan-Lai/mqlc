# Typed IR 在生成之後才解析，不做 constrained decoding

模型照常輸出 Extended JSON pipeline，mqlc 事後把它解析成 typed IR 再做 static check；不用 grammar 或 JSON schema 限制模型的輸出。Constrained decoding 綁定特定後端（各後端支援程度不一，Anthropic API 只支援 JSON schema 而非任意 grammar，pipeline 的 operator 組合也難以用 JSON schema 完整描述），而且 IR 只要漏掉一個 operator，模型就完全寫不出正確答案。事後解析則讓 IR 可以不完整：無法表示的部分保留成 raw stage、照常執行，只是不檢查。IR 的範圍以 train split 的 IR coverage ≥95% 為目標，並以 3,186 條 gold pipeline 的 round-trip 測試守住「序列化回去語意不變」。
