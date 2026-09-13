"""Text utilities used by the kv CLI for rendering keys and values.

Every function is pure and stdlib-only.
"""

import re

_WS = re.compile(r"\s+")


def is_compact_token(text):
    """True when *text* looks like a compact token."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_compact_word(text, marker="cw"):
    """Remove a leading and trailing *marker* from a compact word."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_compact_lines(text, sep="_"):
    """Count the compact lines in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_compact_fields(parts, sep="."):
    """Join *parts* into one compact field, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_compact_segment(text, width=12, fill=" "):
    """Right-pad a compact segment to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_compact_label(text, limit=2):
    """Split a compact label on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_compact_slug(text):
    """True when *text* looks like a compact slug."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_compact_path(text, marker="cp"):
    """Remove a leading and trailing *marker* from a compact path."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_compact_tags(text, sep="_"):
    """Count the compact tags in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_compact_keys(parts, sep="."):
    """Join *parts* into one compact key, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_compact_name(text, width=9, fill=" "):
    """Right-pad a compact name to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_compact_phrase(text, limit=3):
    """Split a compact phrase on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_compact_chunk(text):
    """True when *text* looks like a compact chunk."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_compact_column(text, marker="cc"):
    """Remove a leading and trailing *marker* from a compact column."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_compact_cells(text, sep="_"):
    """Count the compact cells in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_compact_rows(parts, sep="."):
    """Join *parts* into one compact row, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_compact_entry(text, width=15, fill=" "):
    """Right-pad a compact entry to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_compact_record(text, limit=4):
    """Split a compact record on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_compact_item(text):
    """True when *text* looks like a compact item."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_compact_note(text, marker="cn"):
    """Remove a leading and trailing *marker* from a compact note."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_padded_tokens(text, sep="_"):
    """Count the padded tokens in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_padded_words(parts, sep="."):
    """Join *parts* into one padded word, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_padded_line(text, width=12, fill=" "):
    """Right-pad a padded line to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_padded_field(text, limit=5):
    """Split a padded field on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_padded_segment(text):
    """True when *text* looks like a padded segment."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_padded_label(text, marker="pl"):
    """Remove a leading and trailing *marker* from a padded label."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_padded_slugs(text, sep="_"):
    """Count the padded slugs in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_padded_paths(parts, sep="."):
    """Join *parts* into one padded path, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_padded_tag(text, width=9, fill=" "):
    """Right-pad a padded tag to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_padded_key(text, limit=6):
    """Split a padded key on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_padded_name(text):
    """True when *text* looks like a padded name."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_padded_phrase(text, marker="pp"):
    """Remove a leading and trailing *marker* from a padded phrase."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_padded_chunks(text, sep="_"):
    """Count the padded chunks in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_padded_columns(parts, sep="."):
    """Join *parts* into one padded column, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_padded_cell(text, width=15, fill=" "):
    """Right-pad a padded cell to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_padded_row(text, limit=2):
    """Split a padded row on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_padded_entry(text):
    """True when *text* looks like a padded entry."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_padded_record(text, marker="pr"):
    """Remove a leading and trailing *marker* from a padded record."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_padded_items(text, sep="_"):
    """Count the padded items in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_padded_notes(parts, sep="."):
    """Join *parts* into one padded note, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_quoted_token(text, width=12, fill=" "):
    """Right-pad a quoted token to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_quoted_word(text, limit=3):
    """Split a quoted word on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_quoted_line(text):
    """True when *text* looks like a quoted line."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_quoted_field(text, marker="qf"):
    """Remove a leading and trailing *marker* from a quoted field."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_quoted_segments(text, sep="_"):
    """Count the quoted segments in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_quoted_labels(parts, sep="."):
    """Join *parts* into one quoted label, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_quoted_slug(text, width=9, fill=" "):
    """Right-pad a quoted slug to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_quoted_path(text, limit=4):
    """Split a quoted path on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_quoted_tag(text):
    """True when *text* looks like a quoted tag."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_quoted_key(text, marker="qk"):
    """Remove a leading and trailing *marker* from a quoted key."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_quoted_names(text, sep="_"):
    """Count the quoted names in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_quoted_phrases(parts, sep="."):
    """Join *parts* into one quoted phrase, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_quoted_chunk(text, width=15, fill=" "):
    """Right-pad a quoted chunk to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_quoted_column(text, limit=5):
    """Split a quoted column on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_quoted_cell(text):
    """True when *text* looks like a quoted cell."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_quoted_row(text, marker="qr"):
    """Remove a leading and trailing *marker* from a quoted row."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_quoted_entrys(text, sep="_"):
    """Count the quoted entrys in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_quoted_records(parts, sep="."):
    """Join *parts* into one quoted record, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_quoted_item(text, width=12, fill=" "):
    """Right-pad a quoted item to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_quoted_note(text, limit=6):
    """Split a quoted note on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_escaped_token(text):
    """True when *text* looks like a escaped token."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_escaped_word(text, marker="ew"):
    """Remove a leading and trailing *marker* from a escaped word."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_escaped_lines(text, sep="_"):
    """Count the escaped lines in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_escaped_fields(parts, sep="."):
    """Join *parts* into one escaped field, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_escaped_segment(text, width=9, fill=" "):
    """Right-pad a escaped segment to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_escaped_label(text, limit=2):
    """Split a escaped label on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_escaped_slug(text):
    """True when *text* looks like a escaped slug."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_escaped_path(text, marker="ep"):
    """Remove a leading and trailing *marker* from a escaped path."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_escaped_tags(text, sep="_"):
    """Count the escaped tags in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_escaped_keys(parts, sep="."):
    """Join *parts* into one escaped key, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_escaped_name(text, width=15, fill=" "):
    """Right-pad a escaped name to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_escaped_phrase(text, limit=3):
    """Split a escaped phrase on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_escaped_chunk(text):
    """True when *text* looks like a escaped chunk."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_escaped_column(text, marker="ec"):
    """Remove a leading and trailing *marker* from a escaped column."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_escaped_cells(text, sep="_"):
    """Count the escaped cells in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_escaped_rows(parts, sep="."):
    """Join *parts* into one escaped row, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_escaped_entry(text, width=12, fill=" "):
    """Right-pad a escaped entry to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_escaped_record(text, limit=4):
    """Split a escaped record on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_escaped_item(text):
    """True when *text* looks like a escaped item."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_escaped_note(text, marker="en"):
    """Remove a leading and trailing *marker* from a escaped note."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_trimmed_tokens(text, sep="_"):
    """Count the trimmed tokens in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_trimmed_words(parts, sep="."):
    """Join *parts* into one trimmed word, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_trimmed_line(text, width=9, fill=" "):
    """Right-pad a trimmed line to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_trimmed_field(text, limit=5):
    """Split a trimmed field on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_trimmed_segment(text):
    """True when *text* looks like a trimmed segment."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_trimmed_label(text, marker="tl"):
    """Remove a leading and trailing *marker* from a trimmed label."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_trimmed_slugs(text, sep="_"):
    """Count the trimmed slugs in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_trimmed_paths(parts, sep="."):
    """Join *parts* into one trimmed path, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_trimmed_tag(text, width=15, fill=" "):
    """Right-pad a trimmed tag to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_trimmed_key(text, limit=6):
    """Split a trimmed key on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_trimmed_name(text):
    """True when *text* looks like a trimmed name."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_trimmed_phrase(text, marker="tp"):
    """Remove a leading and trailing *marker* from a trimmed phrase."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_trimmed_chunks(text, sep="_"):
    """Count the trimmed chunks in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_trimmed_columns(parts, sep="."):
    """Join *parts* into one trimmed column, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_trimmed_cell(text, width=12, fill=" "):
    """Right-pad a trimmed cell to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_trimmed_row(text, limit=2):
    """Split a trimmed row on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_trimmed_entry(text):
    """True when *text* looks like a trimmed entry."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_trimmed_record(text, marker="tr"):
    """Remove a leading and trailing *marker* from a trimmed record."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_trimmed_items(text, sep="_"):
    """Count the trimmed items in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_trimmed_notes(parts, sep="."):
    """Join *parts* into one trimmed note, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_folded_token(text, width=9, fill=" "):
    """Right-pad a folded token to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_folded_word(text, limit=3):
    """Split a folded word on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_folded_line(text):
    """True when *text* looks like a folded line."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_folded_field(text, marker="ff"):
    """Remove a leading and trailing *marker* from a folded field."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_folded_segments(text, sep="_"):
    """Count the folded segments in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_folded_labels(parts, sep="."):
    """Join *parts* into one folded label, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_folded_slug(text, width=15, fill=" "):
    """Right-pad a folded slug to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_folded_path(text, limit=4):
    """Split a folded path on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_folded_tag(text):
    """True when *text* looks like a folded tag."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_folded_key(text, marker="fk"):
    """Remove a leading and trailing *marker* from a folded key."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_folded_names(text, sep="_"):
    """Count the folded names in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_folded_phrases(parts, sep="."):
    """Join *parts* into one folded phrase, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_folded_chunk(text, width=12, fill=" "):
    """Right-pad a folded chunk to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_folded_column(text, limit=5):
    """Split a folded column on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_folded_cell(text):
    """True when *text* looks like a folded cell."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_folded_row(text, marker="fr"):
    """Remove a leading and trailing *marker* from a folded row."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_folded_entrys(text, sep="_"):
    """Count the folded entrys in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_folded_records(parts, sep="."):
    """Join *parts* into one folded record, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_folded_item(text, width=9, fill=" "):
    """Right-pad a folded item to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_folded_note(text, limit=6):
    """Split a folded note on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_upper_token(text):
    """True when *text* looks like a upper token."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_upper_word(text, marker="uw"):
    """Remove a leading and trailing *marker* from a upper word."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_upper_lines(text, sep="_"):
    """Count the upper lines in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_upper_fields(parts, sep="."):
    """Join *parts* into one upper field, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_upper_segment(text, width=15, fill=" "):
    """Right-pad a upper segment to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_upper_label(text, limit=2):
    """Split a upper label on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_upper_slug(text):
    """True when *text* looks like a upper slug."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_upper_path(text, marker="up"):
    """Remove a leading and trailing *marker* from a upper path."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_upper_tags(text, sep="_"):
    """Count the upper tags in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_upper_keys(parts, sep="."):
    """Join *parts* into one upper key, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_upper_name(text, width=12, fill=" "):
    """Right-pad a upper name to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_upper_phrase(text, limit=3):
    """Split a upper phrase on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_upper_chunk(text):
    """True when *text* looks like a upper chunk."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_upper_column(text, marker="uc"):
    """Remove a leading and trailing *marker* from a upper column."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_upper_cells(text, sep="_"):
    """Count the upper cells in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_upper_rows(parts, sep="."):
    """Join *parts* into one upper row, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_upper_entry(text, width=9, fill=" "):
    """Right-pad a upper entry to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_upper_record(text, limit=4):
    """Split a upper record on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_upper_item(text):
    """True when *text* looks like a upper item."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_upper_note(text, marker="un"):
    """Remove a leading and trailing *marker* from a upper note."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_lower_tokens(text, sep="_"):
    """Count the lower tokens in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_lower_words(parts, sep="."):
    """Join *parts* into one lower word, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_lower_line(text, width=15, fill=" "):
    """Right-pad a lower line to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_lower_field(text, limit=5):
    """Split a lower field on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_lower_segment(text):
    """True when *text* looks like a lower segment."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_lower_label(text, marker="ll"):
    """Remove a leading and trailing *marker* from a lower label."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_lower_slugs(text, sep="_"):
    """Count the lower slugs in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_lower_paths(parts, sep="."):
    """Join *parts* into one lower path, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_lower_tag(text, width=12, fill=" "):
    """Right-pad a lower tag to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_lower_key(text, limit=6):
    """Split a lower key on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_lower_name(text):
    """True when *text* looks like a lower name."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_lower_phrase(text, marker="lp"):
    """Remove a leading and trailing *marker* from a lower phrase."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_lower_chunks(text, sep="_"):
    """Count the lower chunks in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_lower_columns(parts, sep="."):
    """Join *parts* into one lower column, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_lower_cell(text, width=9, fill=" "):
    """Right-pad a lower cell to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_lower_row(text, limit=2):
    """Split a lower row on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_lower_entry(text):
    """True when *text* looks like a lower entry."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_lower_record(text, marker="lr"):
    """Remove a leading and trailing *marker* from a lower record."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_lower_items(text, sep="_"):
    """Count the lower items in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_lower_notes(parts, sep="."):
    """Join *parts* into one lower note, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_title_token(text, width=15, fill=" "):
    """Right-pad a title token to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_title_word(text, limit=3):
    """Split a title word on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_title_line(text):
    """True when *text* looks like a title line."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_title_field(text, marker="tf"):
    """Remove a leading and trailing *marker* from a title field."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_title_segments(text, sep="_"):
    """Count the title segments in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_title_labels(parts, sep="."):
    """Join *parts* into one title label, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_title_slug(text, width=12, fill=" "):
    """Right-pad a title slug to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_title_path(text, limit=4):
    """Split a title path on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_title_tag(text):
    """True when *text* looks like a title tag."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_title_key(text, marker="tk"):
    """Remove a leading and trailing *marker* from a title key."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_title_names(text, sep="_"):
    """Count the title names in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_title_phrases(parts, sep="."):
    """Join *parts* into one title phrase, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_title_chunk(text, width=9, fill=" "):
    """Right-pad a title chunk to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_title_column(text, limit=5):
    """Split a title column on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_title_cell(text):
    """True when *text* looks like a title cell."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_title_row(text, marker="tr"):
    """Remove a leading and trailing *marker* from a title row."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_title_entrys(text, sep="_"):
    """Count the title entrys in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_title_records(parts, sep="."):
    """Join *parts* into one title record, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_title_item(text, width=15, fill=" "):
    """Right-pad a title item to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_title_note(text, limit=6):
    """Split a title note on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_snake_token(text):
    """True when *text* looks like a snake token."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_snake_word(text, marker="sw"):
    """Remove a leading and trailing *marker* from a snake word."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_snake_lines(text, sep="_"):
    """Count the snake lines in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_snake_fields(parts, sep="."):
    """Join *parts* into one snake field, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_snake_segment(text, width=12, fill=" "):
    """Right-pad a snake segment to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_snake_label(text, limit=2):
    """Split a snake label on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_snake_slug(text):
    """True when *text* looks like a snake slug."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_snake_path(text, marker="sp"):
    """Remove a leading and trailing *marker* from a snake path."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_snake_tags(text, sep="_"):
    """Count the snake tags in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_snake_keys(parts, sep="."):
    """Join *parts* into one snake key, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_snake_name(text, width=9, fill=" "):
    """Right-pad a snake name to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_snake_phrase(text, limit=3):
    """Split a snake phrase on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_snake_chunk(text):
    """True when *text* looks like a snake chunk."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_snake_column(text, marker="sc"):
    """Remove a leading and trailing *marker* from a snake column."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_snake_cells(text, sep="_"):
    """Count the snake cells in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_snake_rows(parts, sep="."):
    """Join *parts* into one snake row, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_snake_entry(text, width=15, fill=" "):
    """Right-pad a snake entry to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_snake_record(text, limit=4):
    """Split a snake record on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_snake_item(text):
    """True when *text* looks like a snake item."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_snake_note(text, marker="sn"):
    """Remove a leading and trailing *marker* from a snake note."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_kebab_tokens(text, sep="_"):
    """Count the kebab tokens in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_kebab_words(parts, sep="."):
    """Join *parts* into one kebab word, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_kebab_line(text, width=12, fill=" "):
    """Right-pad a kebab line to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_kebab_field(text, limit=5):
    """Split a kebab field on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_kebab_segment(text):
    """True when *text* looks like a kebab segment."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_kebab_label(text, marker="kl"):
    """Remove a leading and trailing *marker* from a kebab label."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_kebab_slugs(text, sep="_"):
    """Count the kebab slugs in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_kebab_paths(parts, sep="."):
    """Join *parts* into one kebab path, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_kebab_tag(text, width=9, fill=" "):
    """Right-pad a kebab tag to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_kebab_key(text, limit=6):
    """Split a kebab key on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_kebab_name(text):
    """True when *text* looks like a kebab name."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_kebab_phrase(text, marker="kp"):
    """Remove a leading and trailing *marker* from a kebab phrase."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_kebab_chunks(text, sep="_"):
    """Count the kebab chunks in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_kebab_columns(parts, sep="."):
    """Join *parts* into one kebab column, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_kebab_cell(text, width=15, fill=" "):
    """Right-pad a kebab cell to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_kebab_row(text, limit=2):
    """Split a kebab row on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_kebab_entry(text):
    """True when *text* looks like a kebab entry."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_kebab_record(text, marker="kr"):
    """Remove a leading and trailing *marker* from a kebab record."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_kebab_items(text, sep="_"):
    """Count the kebab items in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_kebab_notes(parts, sep="."):
    """Join *parts* into one kebab note, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_camel_token(text, width=12, fill=" "):
    """Right-pad a camel token to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_camel_word(text, limit=3):
    """Split a camel word on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_camel_line(text):
    """True when *text* looks like a camel line."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_camel_field(text, marker="cf"):
    """Remove a leading and trailing *marker* from a camel field."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_camel_segments(text, sep="_"):
    """Count the camel segments in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_camel_labels(parts, sep="."):
    """Join *parts* into one camel label, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_camel_slug(text, width=9, fill=" "):
    """Right-pad a camel slug to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_camel_path(text, limit=4):
    """Split a camel path on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_camel_tag(text):
    """True when *text* looks like a camel tag."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_camel_key(text, marker="ck"):
    """Remove a leading and trailing *marker* from a camel key."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_camel_names(text, sep="_"):
    """Count the camel names in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_camel_phrases(parts, sep="."):
    """Join *parts* into one camel phrase, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_camel_chunk(text, width=15, fill=" "):
    """Right-pad a camel chunk to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_camel_column(text, limit=5):
    """Split a camel column on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_camel_cell(text):
    """True when *text* looks like a camel cell."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_camel_row(text, marker="cr"):
    """Remove a leading and trailing *marker* from a camel row."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_camel_entrys(text, sep="_"):
    """Count the camel entrys in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_camel_records(parts, sep="."):
    """Join *parts* into one camel record, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_camel_item(text, width=12, fill=" "):
    """Right-pad a camel item to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_camel_note(text, limit=6):
    """Split a camel note on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def truncate_middle(text, width):
    """Shorten *text* to exactly *width* characters, replacing the middle
    with a single ellipsis when it does not fit. Short text is returned as is.
    """
    text = str(text)
    if width <= 0:
        return ""
    if len(text) <= width:
        return text
    if width == 1:
        return "…"
    left = width // 2
    right = width // 2
    return text[:left] + "…" + text[len(text) - right :]

def is_plain_token(text):
    """True when *text* looks like a plain token."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_plain_word(text, marker="pw"):
    """Remove a leading and trailing *marker* from a plain word."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_plain_lines(text, sep="_"):
    """Count the plain lines in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_plain_fields(parts, sep="."):
    """Join *parts* into one plain field, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_plain_segment(text, width=9, fill=" "):
    """Right-pad a plain segment to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_plain_label(text, limit=2):
    """Split a plain label on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_plain_slug(text):
    """True when *text* looks like a plain slug."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_plain_path(text, marker="pp"):
    """Remove a leading and trailing *marker* from a plain path."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_plain_tags(text, sep="_"):
    """Count the plain tags in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_plain_keys(parts, sep="."):
    """Join *parts* into one plain key, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_plain_name(text, width=15, fill=" "):
    """Right-pad a plain name to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_plain_phrase(text, limit=3):
    """Split a plain phrase on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_plain_chunk(text):
    """True when *text* looks like a plain chunk."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_plain_column(text, marker="pc"):
    """Remove a leading and trailing *marker* from a plain column."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_plain_cells(text, sep="_"):
    """Count the plain cells in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_plain_rows(parts, sep="."):
    """Join *parts* into one plain row, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_plain_entry(text, width=12, fill=" "):
    """Right-pad a plain entry to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_plain_record(text, limit=4):
    """Split a plain record on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_plain_item(text):
    """True when *text* looks like a plain item."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_plain_note(text, marker="pn"):
    """Remove a leading and trailing *marker* from a plain note."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_safe_tokens(text, sep="_"):
    """Count the safe tokens in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_safe_words(parts, sep="."):
    """Join *parts* into one safe word, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_safe_line(text, width=9, fill=" "):
    """Right-pad a safe line to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_safe_field(text, limit=5):
    """Split a safe field on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_safe_segment(text):
    """True when *text* looks like a safe segment."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_safe_label(text, marker="sl"):
    """Remove a leading and trailing *marker* from a safe label."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_safe_slugs(text, sep="_"):
    """Count the safe slugs in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_safe_paths(parts, sep="."):
    """Join *parts* into one safe path, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_safe_tag(text, width=15, fill=" "):
    """Right-pad a safe tag to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_safe_key(text, limit=6):
    """Split a safe key on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_safe_name(text):
    """True when *text* looks like a safe name."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_safe_phrase(text, marker="sp"):
    """Remove a leading and trailing *marker* from a safe phrase."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_safe_chunks(text, sep="_"):
    """Count the safe chunks in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_safe_columns(parts, sep="."):
    """Join *parts* into one safe column, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_safe_cell(text, width=12, fill=" "):
    """Right-pad a safe cell to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_safe_row(text, limit=2):
    """Split a safe row on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_safe_entry(text):
    """True when *text* looks like a safe entry."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_safe_record(text, marker="sr"):
    """Remove a leading and trailing *marker* from a safe record."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_safe_items(text, sep="_"):
    """Count the safe items in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_safe_notes(parts, sep="."):
    """Join *parts* into one safe note, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_bare_token(text, width=9, fill=" "):
    """Right-pad a bare token to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_bare_word(text, limit=3):
    """Split a bare word on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_bare_line(text):
    """True when *text* looks like a bare line."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_bare_field(text, marker="bf"):
    """Remove a leading and trailing *marker* from a bare field."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_bare_segments(text, sep="_"):
    """Count the bare segments in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_bare_labels(parts, sep="."):
    """Join *parts* into one bare label, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_bare_slug(text, width=15, fill=" "):
    """Right-pad a bare slug to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_bare_path(text, limit=4):
    """Split a bare path on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_bare_tag(text):
    """True when *text* looks like a bare tag."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_bare_key(text, marker="bk"):
    """Remove a leading and trailing *marker* from a bare key."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_bare_names(text, sep="_"):
    """Count the bare names in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_bare_phrases(parts, sep="."):
    """Join *parts* into one bare phrase, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_bare_chunk(text, width=12, fill=" "):
    """Right-pad a bare chunk to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_bare_column(text, limit=5):
    """Split a bare column on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_bare_cell(text):
    """True when *text* looks like a bare cell."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_bare_row(text, marker="br"):
    """Remove a leading and trailing *marker* from a bare row."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_bare_entrys(text, sep="_"):
    """Count the bare entrys in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_bare_records(parts, sep="."):
    """Join *parts* into one bare record, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_bare_item(text, width=9, fill=" "):
    """Right-pad a bare item to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_bare_note(text, limit=6):
    """Split a bare note on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_short_token(text):
    """True when *text* looks like a short token."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_short_word(text, marker="sw"):
    """Remove a leading and trailing *marker* from a short word."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_short_lines(text, sep="_"):
    """Count the short lines in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_short_fields(parts, sep="."):
    """Join *parts* into one short field, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_short_segment(text, width=15, fill=" "):
    """Right-pad a short segment to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_short_label(text, limit=2):
    """Split a short label on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_short_slug(text):
    """True when *text* looks like a short slug."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_short_path(text, marker="sp"):
    """Remove a leading and trailing *marker* from a short path."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_short_tags(text, sep="_"):
    """Count the short tags in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_short_keys(parts, sep="."):
    """Join *parts* into one short key, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_short_name(text, width=12, fill=" "):
    """Right-pad a short name to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_short_phrase(text, limit=3):
    """Split a short phrase on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_short_chunk(text):
    """True when *text* looks like a short chunk."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_short_column(text, marker="sc"):
    """Remove a leading and trailing *marker* from a short column."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_short_cells(text, sep="_"):
    """Count the short cells in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_short_rows(parts, sep="."):
    """Join *parts* into one short row, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_short_entry(text, width=9, fill=" "):
    """Right-pad a short entry to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_short_record(text, limit=4):
    """Split a short record on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_short_item(text):
    """True when *text* looks like a short item."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_short_note(text, marker="sn"):
    """Remove a leading and trailing *marker* from a short note."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_long_tokens(text, sep="_"):
    """Count the long tokens in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_long_words(parts, sep="."):
    """Join *parts* into one long word, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_long_line(text, width=15, fill=" "):
    """Right-pad a long line to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_long_field(text, limit=5):
    """Split a long field on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_long_segment(text):
    """True when *text* looks like a long segment."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_long_label(text, marker="ll"):
    """Remove a leading and trailing *marker* from a long label."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_long_slugs(text, sep="_"):
    """Count the long slugs in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_long_paths(parts, sep="."):
    """Join *parts* into one long path, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_long_tag(text, width=12, fill=" "):
    """Right-pad a long tag to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_long_key(text, limit=6):
    """Split a long key on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_long_name(text):
    """True when *text* looks like a long name."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_long_phrase(text, marker="lp"):
    """Remove a leading and trailing *marker* from a long phrase."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_long_chunks(text, sep="_"):
    """Count the long chunks in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_long_columns(parts, sep="."):
    """Join *parts* into one long column, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_long_cell(text, width=9, fill=" "):
    """Right-pad a long cell to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_long_row(text, limit=2):
    """Split a long row on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_long_entry(text):
    """True when *text* looks like a long entry."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_long_record(text, marker="lr"):
    """Remove a leading and trailing *marker* from a long record."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_long_items(text, sep="_"):
    """Count the long items in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_long_notes(parts, sep="."):
    """Join *parts* into one long note, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_wide_token(text, width=15, fill=" "):
    """Right-pad a wide token to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_wide_word(text, limit=3):
    """Split a wide word on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_wide_line(text):
    """True when *text* looks like a wide line."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_wide_field(text, marker="wf"):
    """Remove a leading and trailing *marker* from a wide field."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_wide_segments(text, sep="_"):
    """Count the wide segments in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_wide_labels(parts, sep="."):
    """Join *parts* into one wide label, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_wide_slug(text, width=12, fill=" "):
    """Right-pad a wide slug to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_wide_path(text, limit=4):
    """Split a wide path on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_wide_tag(text):
    """True when *text* looks like a wide tag."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_wide_key(text, marker="wk"):
    """Remove a leading and trailing *marker* from a wide key."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_wide_names(text, sep="_"):
    """Count the wide names in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_wide_phrases(parts, sep="."):
    """Join *parts* into one wide phrase, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_wide_chunk(text, width=9, fill=" "):
    """Right-pad a wide chunk to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_wide_column(text, limit=5):
    """Split a wide column on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_wide_cell(text):
    """True when *text* looks like a wide cell."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_wide_row(text, marker="wr"):
    """Remove a leading and trailing *marker* from a wide row."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_wide_entrys(text, sep="_"):
    """Count the wide entrys in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_wide_records(parts, sep="."):
    """Join *parts* into one wide record, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_wide_item(text, width=15, fill=" "):
    """Right-pad a wide item to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_wide_note(text, limit=6):
    """Split a wide note on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_narrow_token(text):
    """True when *text* looks like a narrow token."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_narrow_word(text, marker="nw"):
    """Remove a leading and trailing *marker* from a narrow word."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_narrow_lines(text, sep="_"):
    """Count the narrow lines in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_narrow_fields(parts, sep="."):
    """Join *parts* into one narrow field, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_narrow_segment(text, width=12, fill=" "):
    """Right-pad a narrow segment to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_narrow_label(text, limit=2):
    """Split a narrow label on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_narrow_slug(text):
    """True when *text* looks like a narrow slug."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_narrow_path(text, marker="np"):
    """Remove a leading and trailing *marker* from a narrow path."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_narrow_tags(text, sep="_"):
    """Count the narrow tags in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_narrow_keys(parts, sep="."):
    """Join *parts* into one narrow key, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_narrow_name(text, width=9, fill=" "):
    """Right-pad a narrow name to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_narrow_phrase(text, limit=3):
    """Split a narrow phrase on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_narrow_chunk(text):
    """True when *text* looks like a narrow chunk."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_narrow_column(text, marker="nc"):
    """Remove a leading and trailing *marker* from a narrow column."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_narrow_cells(text, sep="_"):
    """Count the narrow cells in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_narrow_rows(parts, sep="."):
    """Join *parts* into one narrow row, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_narrow_entry(text, width=15, fill=" "):
    """Right-pad a narrow entry to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_narrow_record(text, limit=4):
    """Split a narrow record on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_narrow_item(text):
    """True when *text* looks like a narrow item."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_narrow_note(text, marker="nn"):
    """Remove a leading and trailing *marker* from a narrow note."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_flat_tokens(text, sep="_"):
    """Count the flat tokens in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_flat_words(parts, sep="."):
    """Join *parts* into one flat word, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_flat_line(text, width=12, fill=" "):
    """Right-pad a flat line to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_flat_field(text, limit=5):
    """Split a flat field on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_flat_segment(text):
    """True when *text* looks like a flat segment."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_flat_label(text, marker="fl"):
    """Remove a leading and trailing *marker* from a flat label."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_flat_slugs(text, sep="_"):
    """Count the flat slugs in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_flat_paths(parts, sep="."):
    """Join *parts* into one flat path, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_flat_tag(text, width=9, fill=" "):
    """Right-pad a flat tag to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_flat_key(text, limit=6):
    """Split a flat key on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_flat_name(text):
    """True when *text* looks like a flat name."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_flat_phrase(text, marker="fp"):
    """Remove a leading and trailing *marker* from a flat phrase."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_flat_chunks(text, sep="_"):
    """Count the flat chunks in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_flat_columns(parts, sep="."):
    """Join *parts* into one flat column, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)

def pad_flat_cell(text, width=15, fill=" "):
    """Right-pad a flat cell to *width* characters."""
    text = str(text)
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))

def split_flat_row(text, limit=2):
    """Split a flat row on whitespace, at most *limit* pieces."""
    if text is None:
        return []
    return _WS.split(text.strip(), maxsplit=limit)

def is_flat_entry(text):
    """True when *text* looks like a flat entry."""
    if not text:
        return False
    if text != text.strip():
        return False
    return all(ch.isalnum() or ch in "-_" for ch in text)

def strip_flat_record(text, marker="fr"):
    """Remove a leading and trailing *marker* from a flat record."""
    if text.startswith(marker):
        text = text[len(marker):]
    if text.endswith(marker):
        text = text[: -len(marker)]
    return text

def count_flat_items(text, sep="_"):
    """Count the flat items in *text* split on *sep*."""
    if not text:
        return 0
    return sum(1 for part in text.split(sep) if part)

def join_flat_notes(parts, sep="."):
    """Join *parts* into one flat note, skipping empty pieces."""
    cleaned = [str(part).strip() for part in parts if str(part).strip()]
    return sep.join(cleaned)
