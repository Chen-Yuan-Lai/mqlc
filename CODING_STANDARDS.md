# Coding standards

Rules for reviewing a change, each a judgement call that no check can make. Apply every rule to every hunk.

## Glossary words keep their meaning

An identifier, comment or record key that uses a word `GLOSSARY.md` defines means exactly that: a `candidate` is a pipeline the model generated, a `repair` feeds an execution or static check error back, a `sample` is one benchmark question. A new concept gets a word the glossary leaves free. For example, the distinct values value correction reads were first named `CANDIDATES` and became `MAX_VALUES`.

## Text the model reads is quoted in its ADR

A prompt, a repair or value correction feedback, or any other text sent to the model changes scores. The ADR that decides it quotes it word for word (see ADR 0013 and 0014), so a later change to the text is a visible change to a decision.
