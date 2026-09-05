# Fixtures

Every file in this directory is **100% synthetic**. None of it was derived from
a real ChatGPT export or any real user's conversation history — ids, titles,
timestamps, and message content were all authored by hand for this repository.
No real export data is ever committed here.

- `sample-export.json` — three synthetic conversations exercising the main
  cases: a linear thread, a regenerated-answer branch (with an abandoned
  sibling reply), and a short Hebrew/RTL conversation.
- `malformed-export.json` — six synthetic conversations, each carrying one
  kind of recoverable or unusual damage (dangling `current_node`, a broken
  parent link, a two-node parent cycle, an unrecognized `content_type`, a
  multimodal image attachment, and a hostile title with a bidi override and an
  ANSI escape sequence). The file as a whole is still valid JSON.
- `wrapped-export.json` — the alternate `{"conversations": [...]}` top-level
  shape.
- `empty-export.json` — a bare `[]`, the smallest possible valid export.

If you add a fixture, keep it synthetic and obviously fictional.
