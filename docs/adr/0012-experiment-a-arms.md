# 實驗 A 的四個 arm，與人工 MDL 只能用在 train

實驗 A 比較四個 arm（#10）：`official`（官方說明，論文 baseline）、`auto-mdl`（自動 MDL 取代官方說明）、`official-auto-mdl`（官方說明 + 自動 MDL）、`official-evidence`（官方說明 + `mongodb_evidence`，上限）。三個做法會直接影響分數：

**官方說明 + 自動 MDL 逐欄位合併，而不是兩段前後串接。** 兩邊的欄位名稱是同一套點分路徑，所以每個 collection 只有一個 `Collection:` 標頭，每個欄位一行：官方那行原文不動（只去掉頭尾空白），後面接上 MDL 對同一路徑寫的 key（`in_array`、`present`、`references`、`references_matched`、`values`、`format`、`min`、`max`、`common_values`、`untrimmed`，以及人工 MDL 的 `description`、`computed_as`）。`data_type` 以官方為準；MDL 觀察到的型別不同時（例如官方 `Array`、MDL `Array<Document>`）另寫成 `observed_type`，並在 instructions 最後補一條更正：有 `field_description` 的欄位，`data_type` 是官方的型別而不是觀察到的型別（profiler 寫的 instruction 是照 `auto-mdl` 的意思寫的），觀察到的型別在 `observed_type`。只有官方有的欄位照官方原文，只有 MDL 有的欄位接在該 collection 的官方欄位之後、依 MDL 順序、照 `auto-mdl` 的寫法；collection 取兩邊的聯集、依名稱排序。這延續 ADR 0004「arm 之間只差內容、不差形狀」：四個 arm 的 prompt 都是同一種版面，串接則讓每個 collection 出現兩次、prompt 更長，模型還得自己對齊兩份描述。代價是合併規則是我們自訂的，換一種合併方式就得重跑。合併後的 prompt 約為官方說明的 1.35 倍（最大的 works_cycles 約 67k token），全部放得進 200k 的 context。

**沒有 `mongodb_evidence` 的 343 題（dev 140、train 203）照送，不加 Evidence 區塊。** 這些題目在 `official-evidence` 的 prompt 和 `official` 完全相同，照常計分，分母仍是全部題目，所以四個 arm 可以逐題配對比較。不送或另計會讓這一組的分母和其他組不同；送一個空的 `<Evidence>` 區塊則是我們自己發明的提示。Evidence 照資料集原文送出（有 48 題頭尾帶空白），只有去掉空白後為空才算沒有。

**人工 MDL 只能用在 train 資料庫，而且是另外兩個 arm。** `human-mdl` 與 `official-human-mdl` 讀 `--human-mdl-dir`（預設 `mdl/human`），版面與 `auto-mdl`、`official-auto-mdl` 相同；只要題目的資料庫不在 train split，engine 就以 `AskError::Leakage` 拒絕，什麼都不送。依 ADR 0011 這是設定錯誤：`bench` 在寫 manifest 之前就停下、不留紀錄。這把 ADR 0006 的防洩漏規則變成機械檢查：train 與 dev 的資料庫不重疊，所以 dev 資料庫的人工 MDL 不可能「只根據 train 撰寫」；parent spec 也說人工 MDL 不進實驗 A，只用來在 train 上找 profiler 的缺口。檢查認的是 arm 而不是目錄內容：把人工 MDL 放進 `--mdl-dir` 以 `auto-mdl` 跑，檢查擋不住，manifest 的 `knowledge_dir` 會留下它。

Manifest 記錄每組用了哪份知識：`config.arm` 是 arm 名稱，`config.knowledge` 是每個資料庫實際送出的 schema 知識的 hash（官方說明的 arm 是 render 後的官方說明，MDL arm 是 render 後的 MDL，合併 arm 是合併後的文字；`official-evidence` 再加上該資料庫每題的 `sample_id` 與 evidence），每次 invocation 的 `knowledge_dir` 是 MDL 讀自哪個目錄。資料集版本由 `scripts/fetch-data.sh` 鎖定，但 manifest 不記版本，所以官方說明與 evidence 也要記 hash，資料目錄換了版本或被改過時才會拒絕續跑。#10 之前的官方說明 run 沒有這個欄位，續跑時會補上（續跑本來就允許新增資料庫的 hash）。
