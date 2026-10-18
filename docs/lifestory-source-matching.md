# Life story source matching

New transcripts are body-only Markdown. Life Story discovery compares each
GEDCOM person with transcript bodies and relative file paths; it does not
depend on JSON sidecars or YAML frontmatter. Legacy frontmatter is stripped
before matching and before a transcript is supplied to the biography reviewer.

## Signals used

- An exact normalized name in transcript text is a direct candidate match.
- OCR name similarity must reach 78% and must be corroborated by a matching
  GEDCOM birth/event year, birth/event place, or a related person's name.
- A name in a filename or parent-folder path is a candidate only with a
  corroborating year, place, or related name in the transcript/path. A grouped
  name can also be suggested when all name tokens occur in the transcript and
  the full name occurs in the path.
- Similarity compares normalized name tokens using edit distance. It ignores
  case and punctuation, so `Arthur /Astle/` can be compared with `Arthur Aalle`.

These signals generate suggestions, not identity claims. A user must confirm a
candidate before it is included in a biography. Rejected transcript-person
links are excluded for that transcript version. Links are stored in SQLite by
GEDCOM individual identity, relative transcript path, and transcript SHA-256.
The identity key is scoped to the uploaded GEDCOM contents, preventing links
from one tree being reused for another. If a transcript changes, the old
decision is reset to suggested. A previously confirmed, unchanged transcript
remains available even if a later discovery pass does not independently
rediscover it.

For example, `Arthur Aalle` in a transcript under
`Arthur Astle (1870)/1901 Census.md` may be suggested for GEDCOM person
`Arthur /Astle/` when year or family context corroborates the match. The user
reviews the transcription and confirms or rejects it in the audit page before
generating a biography.

## Historical record review

Only confirmed transcripts are supplied to the biography reviewer, together
with their matching clues. The reviewer reads them as evidence and identifies
each source's structure. Its guidance covers civil and parish registers and
indexes, census schedules, probate records, military lists, directories and
electoral rolls, passenger and immigration records, newspapers, school and
employment records, land and court records, cemetery inscriptions, and family
letters, diaries and photographs. It avoids assuming that every source is a
table or that every name is printed in the same form.

In some indexes, a surname is printed only once at the start of a group,
column or page, then only given and middle names appear in following entries.
The reviewer is instructed to carry that surname forward only while the layout
clearly continues the group, stop at a new surname or clear boundary, and
identify a reconstructed full name as a structural reading rather than text
fully printed on that line. This helps relate index entries without silently
merging neighboring people or carrying a surname across columns/pages. The
instructions are in `LIFE_STORY_REVIEW_PROMPT` in `src/web/handlers.rs`; the
Settings prompt controls transcription, not biography review.

## Confidence and citations

Each Life Story section heading must be followed immediately by a line of the
form `**Confidence: 85%**`. The percentage represents the qualitative strength
of the cited evidence for that section, not a calibrated statistical
probability.

Every non-heading, non-confidence line in the generated biography must contain
an inline Markdown citation to one of the confirmed transcript paths. The
server rejects the response without writing it if a subsection has no
confidence score or any biography line lacks an allowed transcript citation.
This structural check ensures citations are present, but cannot prove that a
citation semantically supports the claim; review the generated story and
source scans.

## Limitations

These rules are conservative heuristics, not identity resolution. Filenames,
OCR names, ages, dates, and places can all be wrong or ambiguous. A false
suggestion can still occur when several clues happen to agree, and a valid
source may be missed when its transcript lacks usable evidence. Review every
candidate and cited transcript, especially for people with common names or
close relatives of similar ages.
