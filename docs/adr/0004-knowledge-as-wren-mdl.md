# Knowledge 以 Wren MDL 表示，MongoDB 專屬資訊放 mqlc 命名空間

Profile 的結果和人工補寫的定義都存成 Wren MDL 專案（models、relationships、instructions），而不是自訂格式。MDL 已經是給 LLM 讀的語意層格式，人工可以直接編輯，也能和 Wren 生態互通；代價是它源自關聯式模型，表達不了巢狀路徑、陣列、嵌入文件。這些資訊放在 model / column properties 的 `mqlc` 命名空間（例如 `mqlc.path`、`mqlc.array`、`mqlc.embedded`），檔案仍是合法的 MDL。
