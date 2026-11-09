# 只在 knowledge 超過 context window 一半時才用 RAG

規劃初期決定完全不用 RAG（Claude 的 context 放得下最大的 works_cycles，約 37k tokens），後來推翻：小 context 的本地與開源模型放不下，需要檢索才能跑。折衷是 knowledge mode 預設 auto：完整 knowledge 不超過後端 context window 的 50% 就全部放入（保留 prompt caching，也避免檢索漏掉 collection），超過才改走 RAG。RAG 以 collection 為單位用 BM25 取 top-k，再沿 MDL relationships 外擴一跳；不做 embedding 檢索。高基數欄位的值不靠檢索，交給 value correction。
