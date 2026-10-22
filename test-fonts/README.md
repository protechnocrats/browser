# Test fonts

A fixed font set for tests and the conformance corpus, so goldens do not
depend on the fonts a machine happens to have (Milestone 35, ADR 009). The
browser itself uses the host's fonts (`text::FontSource`); only tests and
`conformance` read these, through `text::DirectoryFonts`.

| File | From | Kept |
|---|---|---|
| `DejaVu{Sans,Serif,SansMono}-*.ttf` | DejaVu 2.37 (the bundled DejaVu Sans's family) | Latin, Greek, Cyrillic, Hebrew, Arabic, punctuation, arrows, maths, shapes — subset with `pyftsubset`, OpenType features kept |
| `Lohit-Devanagari.ttf` | Lohit Devanagari 2.95 | whole |
| `NotoSansSC-Subset.ttf` | Noto Sans SC (Google Fonts), instanced at weight 400 | ASCII, CJK punctuation, full-width forms, and the 1,259 CJK characters of the corpus's Chinese and Japanese pages and the tests |

Licences: `LICENSE_DEJAVU.txt` (Bitstream Vera / DejaVu), `OFL_Lohit.txt` and
`OFL_NotoSansSC.txt` (SIL Open Font License 1.1). The OFL permits
modification and redistribution with the licence; Noto Sans SC's Reserved
Font Name ("Source") is not the subset's name.
