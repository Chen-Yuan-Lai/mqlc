# mqlc

mqlc 把自然語言問題編譯成可驗證的 MongoDB aggregation pipeline，並在 AptMQL-Bench 上評測「自動產生的資料庫知識」與「靜態檢查」的價值。

## 評測

**Sample（題目）**：
AptMQL-Bench 的一筆資料：一個 question、它所屬的資料庫，以及 gold pipeline。以 `sample_id` 識別。
_Avoid_：example、query、row

**Question（問題）**：
sample 裡使用者用自然語言提出的請求，原封不動取自 BIRD。
_Avoid_：prompt、request

**Gold pipeline（標準答案）**：
sample 附帶、已驗證與原始 SQL 結果等價的起始 collection 加 aggregation pipeline。
_Avoid_：ground truth、reference MQL、gold MQL

**Split**：
train（10 個資料庫、1,762 題）或 dev（11 個資料庫、1,424 題）；兩者的資料庫不重疊。
_Avoid_：fold、subset

**Experiment（實驗）**：
為回答一個問題而比較數個 arm 的一組 run，以字母編號（A、B、E、H…）。

**Arm（實驗組）**：
experiment 中被比較的一種設定，例如實驗 A 的「官方說明 + 自動 MDL」。
_Avoid_：知識組、variant、condition

**Run**：
以單一 arm、單一模型跑一批 sample 的一次執行，產出結果與 manifest。

**Manifest**：
記錄一次 run 所有可重現條件（模型、取樣設定、知識模式、prompt hash、程式版本）的紀錄。

**Candidate（候選）**：
模型針對一個 sample 產生的一個 pipeline；N>1 取樣時以序號區分。
_Avoid_：attempt、prediction

**Soft-EX**：
官方評分腳本的執行正確率：預測結果與 gold pipeline 結果的欄位依值配對（不看欄位名），允許預測多出欄位，數值容許誤差。本專案的主要分數。
_Avoid_：accuracy、EX

**Strict-EX**：
與 Soft-EX 相同的比對，但預測不能多出欄位。論文與 Soft-EX 並列報告。

**Exec-Rate**：
預測能被解析且執行時沒有錯誤的比例，不論結果對錯。
_Avoid_：executable rate、valid rate

**Leakage（洩漏）**：
任何依 dev split 內容做的調整：prompt 迭代、人工 MDL、IR 範圍。dev 只用來報分數。

## 知識

**Knowledge（知識）**：
question 以外、為了讓模型理解資料庫而放進 prompt 的所有內容。

**Schema description（官方說明）**：
AptMQL-Bench 隨資料庫附的人寫欄位說明，每個 collection 一份。論文的「無知識」baseline 已包含它。
_Avoid_：schema doc、data dictionary、無知識

**Evidence（外部知識）**：
每個 sample 附帶、以 MongoDB 欄位寫成的解題提示（`mongodb_evidence`），是論文的「外部知識」。不是 BIRD 原本寫給 SQL 的 `evidence`。
_Avoid_：hint、external knowledge（中英混用時）

**MDL**：
以 Wren MDL 格式描述一個資料庫的知識專案：models、relationships、instructions，MongoDB 專屬資訊放在 `mqlc` 命名空間。

**Profile**：
對一個資料庫抽樣、推斷其結構與值域的動作及其結果。

**Auto MDL（自動 MDL）**：
完全由 profile 產生、未經人工修改的 MDL。
_Avoid_：profiler MDL、MDL 草稿

**Human MDL（人工 MDL）**：
在 auto MDL 上由人補寫公式與業務定義後的版本；撰寫時不看 dev 的 question。

**Knowledge mode（知識模式）**：
knowledge 放進 prompt 的方式：full（全部放入）、rag（只放檢索到的 collection）、auto（由 context 大小決定）。

**Collection recall（collection 召回率）**：
rag 模式檢索到的 collection 涵蓋 gold pipeline 所用 collection（起始 collection、`$lookup.from`、`$unionWith`）的比例。

**Context overflow**：
prompt 超過後端宣告的 context window；這種 sample 不送出，另外計數，不算答錯。

## 驗證與修正

**IR**：
pipeline 的型別化表示，用來做 static check，並能還原成等價的 Extended JSON。

**Raw stage**：
IR 無法表示、原樣保留的 stage；含 raw stage 的部分不做 static check。

**IR coverage（IR 覆蓋率）**：
gold pipeline 完全不需要 raw stage 的 sample 比例，以題為單位計算。

**Static check（靜態檢查）**：
執行前對照知識檢查 pipeline：欄位路徑存在、型別相符、`$unwind` 用在陣列上。

**Repair（修正）**：
把錯誤（執行錯誤或 static check 錯誤）回饋給模型重新生成一次；每個 sample 有修正次數上限。
_Avoid_：retry（指「修正」時）、self-correction

**Value correction（值校正）**：
pipeline 中的字面值在資料中不存在時，找出最接近的實際值並請模型重新生成。
_Avoid_：value repair、fuzzy match
