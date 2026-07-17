# URL outline → zoom field test — 2026-07-17

One `aft_outline` fetch was followed by at most one `aft_zoom` fetch per URL; no transient retries. `WORKS` means the returned structure and section text were usable; `DEGRADED` means usable with materially wrong/noisy structure or lookup; `BROKEN` means empty, stub, unsupported, or error. Heading counts are the outline's emitted headings; `many` means the tool output itself was truncated.

| URL | Class | Outline verdict | Zoom verdict | Failure shape / mechanism hypothesis |
|---|---|---|---|---|
| `https://github.com/torvalds/linux/blob/master/README` | DEGRADED | 6; duplicate `README` plus GitHub shell headings | Shell section returned; `README` lookup ambiguous | `symbol 'README' is ambiguous (2 candidates) — zoom a qualified name for its body`; blob HTML shell mixed with file body |
| `https://raw.githubusercontent.com/torvalds/linux/master/README` | WORKS | 15; real README headings | Complete/readable `Quick Start`, `AI Coding Assistant` | — |
| `https://github.com/expressjs/express` | DEGRADED | 40; README plus repeated GitHub `Uh oh!`/metadata headings | Installation complete; `Contributing` ambiguous | `symbol 'Contributing' is ambiguous (2 candidates)`; repository chrome leaks into outline |
| `https://github.com/nodejs/node/releases/tag/v22.0.0` | DEGRADED | 12; release text plus shell/error headings | Release sections readable | GitHub shell contributes `Uh oh!`, `Sorry, something went wrong`, and `No results found` headings |
| `https://gist.github.com/gaearon/6668a1f6986742109c00` | BROKEN | 0 | Not run | `HTTP 404 Not Found fetching https://gist.github.com/gaearon/6668a1f6986742109c00` |
| `https://gist.githubusercontent.com/gaearon/6668a1f6986742109c00/raw/` | BROKEN | 0 | Not run | `HTTP 404 Not Found fetching https://gist.githubusercontent.com/gaearon/6668a1f6986742109c00/raw/` |
| `https://gitlab.com/gitlab-org/gitlab/-/blob/master/README.md` | BROKEN | 0; only an output artifact name, no headings | Not run | Empty outline; likely forge blob shell/content extraction failure |
| `https://gitlab.com/gitlab-org/gitlab/-/raw/master/README.md` | WORKS | 21; real README headings | Installation and `Why should I use GitLab?` complete | — |
| `https://git.sr.ht/~emersion/soju/tree/master/item/README.md` | BROKEN | 0 | Not run | `HTTP 418 I'm a teapot fetching https://git.sr.ht/~emersion/soju/tree/master/item/README.md` |
| `https://codeberg.org/Codeberg/Community/src/branch/main/README.md` | BROKEN | 0; only an output artifact name | Not run | Empty outline; likely forge page shell rather than README |
| `https://codeberg.org/Codeberg/Community/raw/branch/main/README.md` | DEGRADED | 2; real linked headings | Both zooms failed | `Symbol "Community Issue Tracker" not found ... did you mean: [📓 [Community Issue Tracker](https://codeberg.org/Codeberg/Community/issues)]`; markdown-link heading labels are not normalized |
| `https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Array/map` | DEGRADED | 19; real MDN headings | Both requested sections failed lookup | `Symbol "Description" not found ... did you mean: [[Description](#description)]`; rendered-heading/link syntax mismatch |
| `https://docs.rs/serde/latest/serde/` | WORKS | 8; crate docs (one duplicate crate title) | `Design` and `Data formats` complete | — |
| `https://docs.python.org/3/library/itertools.html` | WORKS | 16; real headings, including `¶` labels | Both sections complete/readable | — |
| `https://requests.readthedocs.io/en/latest/user/quickstart/` | WORKS | 22; real docs headings, including `¶` labels | Response and JSON sections complete | — |
| `https://www.mkdocs.org/user-guide/writing-your-docs/` | WORKS | 16; real docs headings, including `` labels | Markdown and fenced-code sections complete | — |
| `https://docusaurus.io/docs` | WORKS | 17; real headings, including `Fast Track ⏱️` | Both sections complete/readable | — |
| `https://doc.rust-lang.org/std/vec/struct.Vec.html` | WORKS | many; large rustdoc API outline | Guarantees and `push` complete/readable | Large but structured output; no observed truncation in zoom |
| `https://docs.astro.build/en/getting-started/` | WORKS | 2; real Astro landing headings | Both sections readable; image references are harmless noise | — |
| `https://en.wikipedia.org/wiki/Markdown` | WORKS | 13; real article headings | CommonMark and GitHub Flavored Markdown complete | — |
| `https://github.com/bitcoin/bitcoin/wiki` | DEGRADED | 27; wiki content plus GitHub repository chrome | Wiki sections complete | GitHub page shell adds `Uh oh!`, navigation, and metadata headings |
| `https://medium.com/` | WORKS | 2; real marketing headings | Title section readable | Homepage, not an article; no content loss observed |
| `https://medium.com/tag/programming` | DEGRADED | 13; topic and repeated story headings | Readable but very noisy/duplicated | Recommendation cards, image alt text, sign-in links, and repeated feed entries leak into section |
| `https://medium.com/gitconnected/i-spent-the-summer-testing-14-ocr-engines-574126f415c8` | DEGRADED | 2; real article title/subtitle | Article text readable, but sign-in controls, image prompts, author cards, and footer trail the section | Medium article HTML includes action placeholders and publication/author boilerplate; content itself is present |
| `https://overreacted.io/a-complete-guide-to-useeffect/` | WORKS | 23; real article headings with `#` and curly punctuation | TLDR and special-character section complete | — |
| `https://simonwillison.substack.com/` | BROKEN | 0 | Not run | `HTTP 404 Not Found fetching https://simonwillison.substack.com/` |
| `https://simonwillison.net/` | WORKS | 16+; real weblog/feed headings | Article excerpt and Highlights complete | Feed sections intentionally contain many entries |
| `https://raw.githubusercontent.com/github/gitignore/main/README.md` | WORKS | 8; real Markdown headings | Both sections complete/readable | — |
| `https://raw.githubusercontent.com/python/cpython/main/Lib/itertools.py` | BROKEN | 0 | Not run | `HTTP 404 Not Found fetching https://raw.githubusercontent.com/python/cpython/main/Lib/itertools.py` |
| `https://www.w3.org/TR/PNG/iso_8859-1.txt` | BROKEN | 0 | Not run | `HTTP 404 Not Found fetching https://www.w3.org/TR/PNG/iso_8859-1.txt` |
| `https://api.github.com/repos/octocat/Hello-World` | DEGRADED | 0 headings; 70+ JSON fields emitted as `var` symbols | Not run | JSON is structurally enumerated but has no section headings; no heading-oriented zoom path |
| `https://hnrss.org/frontpage` | BROKEN | 0 | Not run | `Unsupported content type 'application/xml; charset=utf-8' for https://hnrss.org/frontpage. Supported: text/html, text/markdown, application/json, text/plain; source files via URL path extension (e.g. .rs, .ts, .mjs)` |
| `https://raw.githubusercontent.com/python/cpython/main/Lib/functools.py` | DEGRADED | 0 headings; many Python functions/classes | Both zooms failed | `Symbol "def reduce(function, sequence, /, initial=_initial_missing)" not found ... did you mean: [reduce]`; source signatures are not accepted as emitted symbol names |
| `https://www.gutenberg.org/files/1342/1342-0.txt` | DEGRADED | 0 | Not run | Plain text fetched but no structure was emitted; text-file fallback is absent |
| `https://www.python.org/psf-landing/feed/rss/` | BROKEN | 0 | Not run | `HTTP 404 Not Found fetching https://www.python.org/psf-landing/feed/rss/` |
| `https://developer.mozilla.org/en-US/docs/Web/HTTP/Overview?utm_source=field-test#how_does_http_work` | DEGRADED | 17; real MDN headings; query/fragment did not corrupt outline | Both zooms failed lookup | `Symbol "HTTP flow" not found ... did you mean: [[HTTP flow](#http_flow)]`; fragment/query path is fine, heading normalization is not |
| `http://example.com/` | WORKS | 1; `Example Domain` | Complete/readable | HTTP→HTTPS/redirect path followed successfully |
| `https://git.io/` | DEGRADED | 1; `Git.io` retirement stub | Stub text readable | No redirect target: page says `URL shortening service is no longer accepting new links.` |
| `https://www.nytimes.com/` | DEGRADED | 11; real homepage sections | Top Stories readable but contains photo credits, ads, and feed/navigation noise | No consent wall intercepted this fetch; homepage extraction is usable but noisy |
| `https://en.wikipedia.org/wiki/List_of_programming_languages` | DEGRADED | 28; real A–Z headings | Sections readable, but giant generated table is extremely noisy | Large-page candidate expands image/link tables and repeated navigation; size/truncation handling needs explicit limits |
| `https://www.figma.com/` | DEGRADED | 17; real landing-page headings | Text is readable but polluted by enormous inline `data:image/...;base64` payloads | Script-heavy product page is server-rendered, but media extraction leaks binary data into zoom |
| `https://www.cloudflare.com/` | WORKS | 6; real landing headings | Both sections complete/readable | — |
| `https://www.w3.org/TR/REC-html40/struct/links.html` | DEGRADED | 8; real legacy HTML headings | Both requested sections failed lookup | `Symbol "Internationalization and links" not found ... did you mean: [12.1.5 Internationalization and links]`; numeric heading prefixes are required |
| `https://www.rfc-editor.org/rfc/rfc2616.txt` | DEGRADED | 0 | Not run | Plain RFC text fetched with no heading structure; text fallback absent |
| `https://gist.github.com/simonw/8117ac4376371dd3fc2b5dbce27e0855` | DEGRADED | 7; gist content plus GitHub controls | SVG section complete but GitHub sign-in/action noise trails it | Web gist is usable, but HTML shell contributes controls and an action-error tail |
| `https://gist.githubusercontent.com/simonw/8117ac4376371dd3fc2b5dbce27e0855/raw/` | WORKS | 3; real Markdown headings | Complete/readable SVG source | — |
| `https://www.economist.com/` | DEGRADED | many; feed headings plus one `undefined undefined` | Sections readable but image-heavy | Paywall/consent did not intercept; media URLs and malformed heading metadata leak into the outline |

## Findings

- Raw GitHub, GitLab, Codeberg, and gist URLs are generally the clean path. GitHub blob/repository/release/wiki and web-gist pages instead expose a mixed document: useful content plus shell, `Uh oh!`, metadata, sign-in, or action-error text.
- The most repeatable zoom defect is heading identity. `aft_outline` preserves Markdown link labels, anchors, emoji, punctuation, and sometimes numeric prefixes; `aft_zoom` often requires the decorated/qualified emitted label rather than the human heading an agent naturally selects. This affected MDN, Codeberg, query+fragment MDN, legacy W3C, and Python source signatures.
- HTML extraction is good on static docs, rustdoc, Wikipedia prose, and SSR landing pages. It is not clean on feed/card-heavy pages: Medium, NYT, Economist, GitHub, and Figma leak navigation, repeated cards, image URLs, base64 media, or malformed metadata.
- JSON is enumerated as fields rather than headings. Plain `.txt` and RSS/XML are either structureless or rejected; the exact RSS failure is a content-type allowlist gap. The tested API JSON remained readable only as a field list.
- The selected consent/paywall candidates did not block the fetch in this environment, and Figma still returned SSR text. This test therefore found extraction noise rather than an empty client-rendered page or cookie-wall interstitial.

## Ranked fixes (weighted by agent exposure, not test count)

1. **Normalize heading identities across outline → zoom.** Preserve a stable internal heading ID and accept the displayed text, Markdown link label, normalized punctuation/emoji, numeric prefix, anchor, and an unambiguous qualified path. This is the highest-frequency failure across documentation and repo pages.
2. **Detect and isolate forge shells from file content.** Prefer provider raw/content endpoints for GitHub/GitLab/Codeberg/sourcehut and identify blob HTML that is only a shell, rather than returning shell headings as if they were document structure.
3. **Add robust text/JSON/XML/RSS fallbacks.** For `text/plain`, emit bounded line/paragraph chunks; for JSON, expose a readable root/value view; for XML/RSS, parse text or return a useful structured error instead of rejecting `application/xml` outright.
4. **Strip boilerplate and binary media from HTML sections.** Remove nav/action/sign-in/footer/card duplication and never expose base64/data-URI payloads as section text; retain alt text and meaningful captions.
5. **Make redirects and fetch failures explicit and useful.** Preserve the final URL and redirect chain, distinguish a retired shortener from a failed redirect, and surface provider status/body for 404/418/403 cases.
6. **Bound large-page extraction without losing section locality.** Stream or cap per-section output with an explicit truncation marker; large rustdoc/Wikipedia/table-heavy pages are common enough to need predictable limits.
7. **Add browser-rendered/consent-aware fallback only after static cleanup.** SPA and consent behavior was not reproduced here, but product pages remain a likely residual class when server HTML lacks the user-visible content.
