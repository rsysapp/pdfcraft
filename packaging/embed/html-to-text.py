"""Plain text from a simple HTML document: html-to-text.py <in.html> <out.txt>

Used by package.sh for the Rust toolchain's COPYRIGHT-library.html. Standard library only, and
the same input always gives the same output (no timestamps, no locale). Text inside <pre> keeps
its layout; elsewhere runs of whitespace are one space.
"""
import re
import sys
from html.parser import HTMLParser

BLOCK_TAGS = {
    "p", "div", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "li", "pre", "details",
    "summary", "table", "tr", "br", "hr", "body", "section",
}
SKIPPED_TAGS = {"script", "style", "head", "title"}


class TextExtractor(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.parts = []
        self.pre_depth = 0
        self.skip_depth = 0

    def handle_starttag(self, tag, attrs):
        if tag in SKIPPED_TAGS:
            self.skip_depth += 1
        if tag == "pre":
            self.pre_depth += 1
        if tag in BLOCK_TAGS:
            self.parts.append("\n\n" if tag.startswith("h") or tag == "pre" else "\n")
        if tag == "li":
            self.parts.append("- ")

    def handle_endtag(self, tag):
        if tag in SKIPPED_TAGS:
            self.skip_depth -= 1
        if tag == "pre":
            self.pre_depth -= 1
        if tag in BLOCK_TAGS:
            self.parts.append("\n\n" if tag.startswith("h") or tag == "pre" else "\n")

    def handle_data(self, data):
        if self.skip_depth:
            return
        if self.pre_depth:
            self.parts.append(data)
            return
        collapsed = re.sub(r"\s+", " ", data)
        # A block tag already started a line, so a space after it is only the markup's layout.
        if not self.parts or self.parts[-1].endswith("\n"):
            collapsed = collapsed.lstrip()
        self.parts.append(collapsed)


def to_text(html_text):
    extractor = TextExtractor()
    extractor.feed(html_text)
    extractor.close()
    text = "".join(extractor.parts)
    lines = [line.rstrip() for line in text.split("\n")]
    collapsed = re.sub(r"\n{3,}", "\n\n", "\n".join(lines))
    return collapsed.strip("\n") + "\n"


if __name__ == "__main__":
    with open(sys.argv[1], encoding="utf-8") as source:
        converted = to_text(source.read())
    with open(sys.argv[2], "w", encoding="utf-8", newline="\n") as target:
        target.write(converted)
