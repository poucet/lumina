#### Kyutai hears the whole sentence, every time (`VA-KYUTAI`) · 2026-09-26 03:13 · lunzunsp

- **The last word stays in its sentence.** Kyutai's end of turn fired while
  the last word was still inside the model's 0.5 s ASR delay, so "…tell me
  what failed" arrived as two turns, "…tell me what" and "failed". The final
  now waits out the delay and carries every word; it arrives ~0.5 s after the
  end-of-turn head fires (end of speech → final 1.2–1.8 s on the fixtures).
- **Each session starts clean.** `stream()` and `transcribe()` reset the
  model first, so one loaded `KyutaiSttProvider` serves any number of
  sessions; before, the previous session's audio bent the next transcript
  ("cargo clippy" → "cargo" + "clicky").
- **Closing a stream mid-sentence keeps the tail**: dropping the audio sender
  flushes the delay with silence and sends the rest as one last final.
  Callers no longer push their own flush silence or hold the final.
