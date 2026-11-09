# 只用 train 調整，dev 只報分數

AptMQL-Bench 沒有隱藏的 test split，dev 就是最終報告的分數，所以任何依 dev 內容做的調整都算 leakage：prompt 迭代、human MDL 撰寫、IR operator 範圍，都只能看 train split；human MDL 只依 schema 與資料撰寫，不看 dev 題目。README 另列全量 3,186 題的分數，供與論文對照。
