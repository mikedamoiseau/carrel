# MOBI test fixtures

Alice's Adventures in Wonderland, Project Gutenberg eBook #11 (public domain;
the Project Gutenberg header is left intact in both files). Used by the
fixture-gated tests in `carrel-core/src/mobi/`.

| File | Source | Exercises | SHA-256 |
|---|---|---|---|
| `alice.mobi` | https://www.gutenberg.org/cache/epub/11/pg11-images-kf8.mobi | KF8 (AZW3, MOBI file version 8) | `d8211269aed55ded8302f289950936db8368b4d4d384c463b722034152f8f1d2` |
| `alice-legacy.mobi` | https://www.gutenberg.org/cache/epub/11/pg11.mobi | legacy Mobipocket (version 6) | `c6de8f83459904177eac2623aa653ef57483ed6968078c8fc3d3171f20e06408` |

These are committed rather than downloaded because Project Gutenberg keeps
regenerating the KF8 file (reissued repeatedly between July and October 2026),
which changed its hash and broke CI each time. Do not re-fetch or replace them
without verifying the new file is still KF8 / version 8 and that the MOBI tests
pass.
