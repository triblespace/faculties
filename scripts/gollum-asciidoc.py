"""Render inert Gollum AsciiDoc as Typst. No file, include, shell or network operations."""
import re


def _string(value):
    chars = []
    for char in value:
        if char in ('"', '\\'):
            chars.append('\\' + char)
        elif char == '\n':
            chars.append('\\n')
        elif char == '\t':
            chars.append('\\t')
        elif ord(char) < 32 or ord(char) == 127:
            chars.append(r'\u{' + format(ord(char), 'x') + '}')
        else:
            chars.append(char)
    return '"' + ''.join(chars) + '"'


def _text(value):
    return '#text(' + _string(value) + ')' if value else ''


# A resolver may return an embedded image as an opaque site marker that it
# expands later; block-level handling below only needs to recognise one.
SITE_OPEN = '\ue000'
_IMAGE_FILE = re.compile(r'\.(?:png|jpe?g|gif|webp|svg)$', re.I)


def _image_attributes(label):
    """Alt text and title of an AsciiDoc image macro's attribute list."""
    positional = []
    named = {}
    for part in re.findall(r'(?:[^,"]|"[^"]*")+', label):
        part = part.strip()
        key = re.match(r'^([A-Za-z][\w-]*)\s*=\s*(.*)$', part)
        if key:
            named[key[1].lower()] = key[2].strip().strip('"')
        else:
            positional.append(part.strip('"'))
    alt = named.get('alt', positional[0] if positional else '')
    return alt, named.get('title')


def _raw(value, block=False):
    return '#raw(' + _string(value) + (', block: true)' if block else ')')


_TOKEN = re.compile(
    r'(?P<wiki>\[\[(?P<wbody>[^\]\n]+)\]\])'
    r'|(?P<macro>\b(?:link:|xref:|image::?|mailto:|https?://)(?P<address>[^\s\[\]]+)\[(?P<label>[^\]\n]*)\])'
    r'|(?P<url>\bhttps?://[^\s<>\[\]"]+)'
    r'|(?P<code>\x60(?P<ctext>[^\x60\n]+)\x60)'
    r'|(?P<strong>(?<!\w)\*(?P<stext>\S(?:[^*\n]*?\S)?)\*(?!\w))'
    r'|(?P<emph>(?<!\w)_(?P<etext>\S(?:[^_\n]*?\S)?)_(?!\w))'
)


def _inline(value, resolve_link, caption=None):
    """Render one line. `caption` (Typst, with the old rendering of the block
    title that carried it) applies to a block image macro that is shown."""
    def link(target, label, display=None):
        if resolve_link:
            if display is not None:
                return resolve_link(target, label, display)
            return resolve_link(target, label)
        if target.lower().startswith(('http://', 'https://', 'mailto:')):
            return '#link(' + _string(target) + ')[' + _text(label) + ']'
        return _text(label if target == label else label + ' (' + target + ')')

    output = []
    end = 0
    for m in _TOKEN.finditer(value):
        output.append(_text(value[end:m.start()]))
        end = m.end()
        if m['wiki']:
            parts = [part.strip() for part in m['wbody'].split('|')]
            if parts[0].lower().startswith('include:') or parts[0] == '_TOC_':
                output.append(_text(m[0]))
            elif re.search(r'\.(?:pdf|jpe?g|png|gz|swi)$', parts[0], re.I):
                label = next((p[4:] for p in parts[1:] if p.startswith('alt=')), parts[0])
                output.append(link(parts[0], label, _tag_display(parts)))
            elif len(parts) <= 2 and parts[-1]:
                output.append(link(parts[-1], parts[0], _tag_display(parts)))
            else:
                output.append(_text(m[0]))
        elif m['macro']:
            prefix = m['macro'][:m['macro'].index(':') + 1].lower()
            target = m['address']
            if prefix in ('http:', 'https:'):
                target = prefix + '//' + target
            elif prefix == 'mailto:':
                target = prefix + target
            label = m['label']
            display = None
            if prefix == 'image:':
                alt, title = _image_attributes(label)
                display = dict(target=target, alt=alt)
                if m['macro'].lower().startswith('image::'):
                    if title:
                        display['caption'] = title
                    elif caption is not None:
                        display['caption_typst'], display['legacy_prefix'] = caption
            if prefix == 'image:' and ',' in label:
                label = label.partition(',')[0].strip('" ')
            output.append(link(target, label or target, display))
        elif m['url']:
            target = m['url'].rstrip('.,;!')
            output.append(link(target, target) + _text(m['url'][len(target):]))
        elif m['code']:
            output.append(_raw(m['ctext']))
        elif m['strong']:
            output.append('#strong[' + _text(m['stext']) + ']')
        else:
            output.append('#emph[' + _text(m['etext']) + ']')
    output.append(_text(value[end:]))
    return ''.join(output)


def _tag_display(parts):
    """A Gollum tag naming an image file is shown as that image."""
    if not _IMAGE_FILE.search(parts[0].split('?', 1)[0]):
        return None
    alt = next((p[4:] for p in parts[1:] if p.startswith('alt=')), '')
    return dict(target=parts[0], alt=alt, caption=alt if 'frame' in parts[1:] and alt else None)


# A literal pipe, code span, Gollum link or bracketed link label can contain a
# pipe of its own. Only standalone separator tokens start a new table cell.
_TABLE_TOKENS = re.compile(r'\\\||\[\[[^\]\n]*\]\]|\[[^\]\n]*\]|`[^`\n]*`|\|')


def _cells(line):
    cells = []
    start = 1
    for token in _TABLE_TOKENS.finditer(line, start):
        if token[0] == '|':
            cells.append(line[start:token.start()].strip().replace(r'\|', '|'))
            start = token.end()
    cells.append(line[start:].strip().replace(r'\|', '|'))
    return cells


def _table(lines, resolve_link, caption=None):
    cells = []
    columns = 0
    blank = False
    for line in lines:
        line = line.strip()
        if not line:
            blank = True
            continue
        if line.startswith('|'):
            group = _cells(line)
            if not columns:
                columns = len(group)
            occupied = len(cells) % columns
            if occupied and occupied + len(group) > columns:
                # A new grouped row does not fit in the old one. Preserve
                # its column positions and leave the missing cells empty.
                cells.extend([''] * (columns - occupied))
            cells.extend(group)
        elif cells:
            cells[-1] += ('\n\n' if blank else '\n') + line
        else:
            return _literal_lines(lines, resolve_link)
        blank = False
    if not cells:
        return ''
    cells.extend([''] * (-len(cells) % columns))
    rendered = []
    for cell in cells:
        paragraphs = cell.split('\n\n')
        body = '\n\n'.join(_literal_lines(paragraph.split('\n'), resolve_link) for paragraph in paragraphs)
        rendered.append('[' + body + ']')
    # PSV tables without explicit attributes have an implicit header only
    # when the first physical row is immediately followed by a blank line.
    if len(lines) > 1 and lines[0].strip() and not lines[1].strip():
        rendered[:columns] = ['table.header(' + ', '.join(rendered[:columns]) + ')']
    table = 'table(columns: ' + str(columns) + ', inset: 4pt,\n' + ',\n'.join(rendered) + '\n)'
    if caption is not None:
        return '#figure(' + table + ', caption: figure.caption(position: top)[' + _inline(caption, resolve_link) + '])'
    return '#' + table


def _literal_lines(lines, resolve_link):
    return '\n#linebreak()\n'.join(_inline(line, resolve_link) for line in lines)


def convert_asciidoc(text, resolve_link=None):
    """Return Typst. resolve_link(target, plain_label) returns trusted generated Typst."""
    lines = text.replace('\r\n', '\n').replace('\r', '\n').split('\n')
    output = []
    i = 0
    ticks = chr(96) * 3
    while i < len(lines):
        line = lines[i]
        i += 1
        mark = line.strip()
        # A block title is attached to its table when the delimiter follows
        # immediately; a dot-prefixed paragraph elsewhere stays ordinary text.
        titled = (re.match(r'^\.\S', mark) and i < len(lines)
                  and re.fullmatch(r'\|={3,}', lines[i].strip()))
        delimiter = lines[i].strip() if titled else mark
        if re.fullmatch(r'\|={3,}', delimiter):
            start = i + 1 if titled else i
            end = next((n for n in range(start, len(lines)) if lines[n].strip() == delimiter), None)
            if end is not None:
                output.append(_table(lines[start:end], resolve_link, mark[1:] if titled else None) + '\n')
                i = end + 1
                continue
        if mark.startswith(ticks) or mark in ('----', '....', '++++', '====', '____', '////'):
            close = ticks if mark.startswith(ticks) else mark
            end = next((n for n in range(i, len(lines)) if lines[n].strip() == close), None)
            if end is not None:
                # Listing/source/passthrough/comment blocks remain inert and visible.
                output.append(_raw('\n'.join(lines[i:end]), True) + '\n')
                i = end + 1
                continue
        if (line.startswith('.') and re.match(r'^\.[^.\s]', line) and i < len(lines)
                and re.fullmatch(r'image::[^\s\[\]]+\[[^\]\n]*\]', lines[i].rstrip())):
            # A block title above a block image is its caption when the image
            # is shown; otherwise both lines render exactly as before.
            title = _inline(line, resolve_link)
            caption = _inline(line[1:], resolve_link)
            if SITE_OPEN not in title + caption:
                image = _inline(lines[i], resolve_link, (caption, title + '\n'))
                if image.startswith(SITE_OPEN):
                    output.append(image)
                    i += 1
                    continue
                output.append(title)
                output.append(image)
                i += 1
                continue
        if not mark:
            output.append('')
        elif re.match(r'^\s*(?:include::|ifdef::|ifndef::|endif::|image::.*\[.*\].*\S\s*$)',line) and not mark.startswith('image::'):
            output.append(_raw(line))
        elif line.startswith(('://','//',':','[source', '[cols','[options','[width')):
            output.append(_raw(line))
        elif (h := re.match(r'^(={1,6})\s+(.+)$', line)):
            output.append(h[1] + ' ' + _inline(h[2], resolve_link))
        elif (item := re.match(r'^(\s*)([*]+|[.]+|[-+])\s+(.+)$', line)):
            marker = '+' if item[2].startswith('.') else '-'
            indent = len(item[1].expandtabs(2)) // 2 + len(item[2]) - 1
            label = item[3]
            if label.startswith(('[ ] ', '[x] ', '[X] ')):
                label = ('☐ ' if label[1]==' ' else '☑ ') + label[4:]
            output.append('  ' * min(indent,8) + marker + ' ' + _inline(label, resolve_link))
        elif line[:1].isspace():
            output.append(_raw(line, True))
        else:
            output.append(_inline(line, resolve_link))
    return '\n'.join(output) + '\n'
