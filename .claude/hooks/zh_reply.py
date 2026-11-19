#!/usr/bin/env python3
"""Stop hook: send a reply back for rewriting when it is not in Chinese (CLAUDE.md 語言)."""
import json
import re
import sys

MIN_UNITS = 10  # CJK characters plus Latin words; shorter replies are not judged
MIN_CJK_SHARE = 0.25


def last_reply(event):
    if event.get("last_assistant_message"):
        return event["last_assistant_message"]
    text = ""
    with open(event["transcript_path"], encoding="utf-8") as transcript:
        for line in transcript:
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            message = entry.get("message")
            if entry.get("type") == "user" and isinstance(message, dict) and isinstance(message.get("content"), str):
                text = ""  # a new prompt: only this turn's replies count
            if entry.get("type") != "assistant" or not isinstance(message, dict):
                continue
            parts = [p.get("text", "") for p in message.get("content", []) if isinstance(p, dict) and p.get("type") == "text"]
            if any(parts):
                text = "\n".join(parts)
    return text


def cjk_share(text):
    text = re.sub(r"```.*?```", "", text, flags=re.S)
    text = re.sub(r"`[^`]*`|https?://\S+", "", text)
    cjk = len(re.findall(r"[㐀-鿿豈-﫿]", text))
    # a Latin word carries about as much as one CJK character
    latin = len(re.findall(r"[A-Za-z]+", text))
    return cjk, latin


def main():
    event = json.load(sys.stdin)
    if event.get("stop_hook_active"):
        return
    cjk, latin = cjk_share(last_reply(event))
    if cjk + latin < MIN_UNITS or cjk / (cjk + latin) >= MIN_CJK_SHARE:
        return
    print(json.dumps({
        "decision": "block",
        "reason": "CLAUDE.md：回答使用者一律使用繁體中文。把剛才的回覆用繁體中文重寫一次（程式碼、指令、識別字照原文）。",
    }, ensure_ascii=False))


if __name__ == "__main__":
    main()
