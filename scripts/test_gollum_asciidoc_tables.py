#!/usr/bin/env python3
"""Exercise native table layout without publishing or loading workspace data."""
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("adapter", Path(__file__).with_name("gollum-asciidoc.py"))
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)


class AsciiDocTables(unittest.TestCase):
    def test_empty_cells_and_short_rows_keep_their_columns(self):
        source = """|====
|A |B |C
|one ||three
|four |five |
|six |seven
|eight |nine |ten
|eleven
|twelve
|thirteen
|last
|====
"""
        body = adapter.convert_asciidoc(source)
        cells = ['A', 'B', 'C', 'one', '', 'three', 'four', 'five', '',
                 'six', 'seven', '', 'eight', 'nine', 'ten', 'eleven', 'twelve', 'thirteen', 'last', '', '']
        expected = ',\n'.join('[' + adapter._text(value) + ']' for value in cells)
        self.assertIn('#table(columns: 3, inset: 4pt,\n' + expected + '\n)', body)
        self.assertNotIn('table.header', body)

    def test_continuations_and_inline_pipes_belong_to_the_cell(self):
        source = """|===
|Keys |Meaning
|[[Shown|Page]] |first line
second line

second paragraph
|a\\|b |`left|right`
|link:manual.pdf[see|manual] |done
|==="""
        calls = []

        def link(target, label):
            calls.append((target, label))
            return '#link("https://example.org")[' + adapter._text(label) + ']'

        body = adapter.convert_asciidoc(source, link)
        self.assertEqual(calls, [('Page', 'Shown'), ('manual.pdf', 'see|manual')])
        self.assertIn('[#text("first line")\n#linebreak()\n#text("second line")\n\n#text("second paragraph")]', body)
        self.assertIn('[#text("a|b")],\n[#raw("left|right")]', body)

    def test_title_and_implicit_header_are_native_typst(self):
        source = """.Reference
|================
|Part |Status

|Alpha |Ready
|Beta |
|================"""
        body = adapter.convert_asciidoc(source)
        self.assertIn('#figure(table(columns: 2', body)
        self.assertIn('caption: figure.caption(position: top)[#text("Reference")]', body)
        self.assertIn('table.header([#text("Part")], [#text("Status")])', body)
        if shutil.which("typst"):
            with tempfile.TemporaryDirectory() as folder:
                page = Path(folder) / "table.typ"
                page.write_text("#show table: it => { [#metadata(1) <observed-table>]; it }\n" + body)
                subprocess.run(["typst", "compile", str(page), str(page.with_suffix(".pdf"))], check=True)
                result = subprocess.check_output(["typst", "query", str(page), "<observed-table>"], text=True)
                self.assertEqual(len(json.loads(result)), 1)


if __name__ == '__main__':
    unittest.main()
