---
status: proposed
---

# 官方說明的串接格式由我們自訂

論文與官方 repo 只公開了 prompt 模板（`{schema_description}` 佔位符）和每個 collection 一份的 schema description 檔，沒有說明多個 collection 怎麼串成一段。我們的做法：依檔名排序，每份前面加一行 `Collection: <name>`，檔案內容只去掉頭尾空白，段落間空一行。這會直接影響 baseline 能否重現論文的 Soft-EX；之後若要改，所有已跑的分數都得重跑。Baseline（#5）若落在論文 47.71 ±3 分內就改為 accepted；若偏離，這是第一個要比對的地方。
