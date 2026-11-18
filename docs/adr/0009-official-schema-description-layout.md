---
status: accepted
---

# 官方說明的串接格式由我們自訂

論文與官方 repo 只公開了 prompt 模板（`{schema_description}` 佔位符）和每個 collection 一份的 schema description 檔，沒有說明多個 collection 怎麼串成一段。我們的做法：依檔名排序，每份前面加一行 `Collection: <name>`，檔案內容只去掉頭尾空白，段落間空一行。這會直接影響 baseline 能否重現論文的 Soft-EX；之後若要改，所有已跑的分數都得重跑。Baseline（#5）若落在論文 47.71 ±3 分內就改為 accepted；若偏離，這是第一個要比對的地方。

Baseline（#5）全量 Soft-EX 41.93，比論文低 5.78 分，超出 ±3，所以照約定先比對這裡。官方 prompt 說每行有 `field_name`、`field_description`、`data_format`、`value_description`，檔案卻是 `data_type` 外加 `required`；把每行改成 prompt 寫的四個 key（`--arm official-prompt-keys`）在 train 1,762 題上與現行格式配對相差 −0.28 ± 1.19 分（95%），沒有顯著差異。這只測了每行的 key；排序、`Collection:` 標頭與空行都沒有變動過，而「不顯著」也不代表完全沒有影響。但目前沒有證據顯示格式造成偏差，所以現行格式改為 accepted，已跑的分數不必重跑。

#19 再測了一種串接方式：每個 collection 用 `<collection name="...">` 包起來，取代 `Collection:` 標頭（`--arm official-tagged`），在 train 上配對相差 −0.28 ± 1.10 分，同樣沒有顯著差異。
