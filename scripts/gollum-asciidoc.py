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


def _inline(value, resolve_link):
    def link(target, label):
        if resolve_link:
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
                output.append(link(parts[0], label))
            elif len(parts) <= 2 and parts[-1]:
                output.append(link(parts[-1], parts[0]))
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
            if prefix == 'image:' and ',' in label:
                label = label.partition(',')[0].strip('" ')
            output.append(link(target, label or target))
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


def _table(lines, resolve_link):
    rows = []
    for line in lines:
        if not line.strip():
            continue
        line = line.strip()
        if not line.startswith('|'):
            return _literal_lines(lines, resolve_link)
        # Protect cell separators inside Gollum links, link labels and verbatim spans.
        parts = re.split(r'(?<!\\)\|(?!(?:[^\[\]]*?\]\]))(?!(?:[^\[\]]*?\]))', line[1:])
        if parts and not parts[-1].strip() and line.endswith('|'):
            parts.pop()
        rows.append([cell.strip().replace(r'\|', '|') for cell in parts])
    widths = {len(row) for row in rows}
    if not rows or len(widths) != 1 or not (1 <= len(rows[0]) <= 8):
        return _literal_lines(lines, resolve_link)
    cells = ['[' + _inline(cell, resolve_link) + ']' for row in rows for cell in row]
    return '#table(columns: ' + str(len(rows[0])) + ', inset: 4pt,\n' + ',\n'.join(cells) + '\n)'


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
        if re.fullmatch(r'\|={3,}', mark):
            end = next((n for n in range(i, len(lines)) if lines[n].strip() == mark), None)
            if end is not None:
                output.append(_table(lines[i:end], resolve_link) + '\n')
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
