# Life story source matching

GeneSleuth selects transcript files for a Life Story biography by comparing the
GEDCOM subject with transcript frontmatter and the records' relative file paths.
This is intended to recover plausible OCR spelling errors without silently
treating every similar name as the same person.

## Signals used

- An exact normalized name in `primary_individuals` remains a direct match.
- An exact name in a filename or parent-folder path is accepted only when a
  GEDCOM birth year or event year also appears in the path or extracted dates.
- A non-exact name needs both a similarity score of at least 78% and independent
  corroboration. The current corroborating signals are a matching birth year, a
  transcript age compatible with the GEDCOM birth year and record year (allowing
  one year of age/date uncertainty), a compatible birthplace, or a GEDCOM event
  year in the transcript dates or path.
- Name similarity compares normalized name tokens using edit distance. It
  ignores case and punctuation, so GEDCOM forms such as `Arthur /Astle/` can be
  compared with `Arthur Aalle`.

For example, `Arthur Aalle` in transcript frontmatter can be included for GEDCOM
person `Arthur /Astle/` when the transcript is under a path such as
`Arthur Astle (1870)/1901 Census.md`: the filename supplies an exact name and
the path's birth year corroborates the person. Transcript age and birthplace
can provide additional evidence.

Each selected source is supplied to Gemini with the matching reasons. Gemini is
instructed to treat those reasons as heuristic rather than proof, compare the
record's other details, and express uncertainty when evidence conflicts. The
audit page lists selected transcripts and the reasons they matched.

## Historical record review

The Life Story reviewer reads the supplied transcripts as evidence and first
identifies each source's structure. Its guidance covers civil and parish
registers and indexes, census schedules, probate records, military lists,
directories and electoral rolls, passenger and immigration records,
newspapers, school and employment records, land and court records, cemetery
inscriptions, and family letters, diaries and photographs. It avoids assuming
that every source is a table or that every name is printed in the same form.

In particular, some indexes print a surname only once at the start of a group,
column or page, then give only the first and middle names for subsequent
entries. The reviewer is instructed to carry that surname forward only while
the layout clearly continues the group, to stop at a new surname or clear
boundary, and to identify any reconstructed full name as a structural reading
rather than text fully printed on that line. This helps relate index entries
without silently merging neighboring people or carrying a surname across
columns/pages. The instructions are in `LIFE_STORY_REVIEW_PROMPT` in
`src/web/handlers.rs`; the separate Settings prompt controls transcription, not
this biography-review prompt.

## Confidence and citations

Each Life Story section heading must be followed immediately by a line of the
form `**Confidence: 85%**`. The percentage represents the qualitative strength
of the cited evidence for that section, not a calibrated statistical
probability.

Every non-heading, non-confidence line in the generated biography must contain
an inline Markdown citation to one of the selected transcript paths. The server
rejects the response without writing it if a subsection has no confidence score
or any biography line lacks an allowed transcript citation. This structural
check ensures citations are present, but it cannot prove that a citation
semantically supports the claim; review the generated story and source scans.

## Limitations

These rules are conservative heuristics, not identity resolution. Filenames,
OCR names, ages, dates, and places can all be wrong or ambiguous. A false match
can still occur when several clues happen to agree, and a valid source may be
missed when its transcript lacks usable metadata. Review the selected-source
list and cited transcript before relying on a biography, especially for people
with common names or close relatives of similar ages.
