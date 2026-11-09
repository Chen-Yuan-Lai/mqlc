# 寬容解析模型回覆

官方 prompt 要求模型輸出 Python 程式碼（`COLLECTION = ...`、`AGGREGATION_PIPELINE = [...]`），論文沒有說明怎麼把它轉成 pipeline。我們不執行模型產生的 Python（安全考量），而是把它當成接近 JSON 的文字解析，並容忍模型常見的 Python 寫法：`True`/`False`/`None`、單引號與 raw string、`\d` 這類 Python 才保留反斜線的跳脫、`#` 註解、結尾逗號；最後以 Extended JSON 轉成 BSON。嚴格的 JSON 解析會把語意正確的答案判成錯，讓分數低估模型；代價是我們的「可解析率」可能與論文不同，比對 baseline 時要一併考慮。
