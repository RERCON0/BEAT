"""Check repository documentation links without network access."""
from pathlib import Path
import re
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
DOCUMENTS = ["README.md", "SECURITY.md", "docs/REFERENCE.md", "docs/AUDIT-2026-10-07.md",
             "icons/README.md", "third_party/README.md", "vendor/symphonia-core/BEAT-PATCH.md"]


def headings(path):
    text = path.read_text(encoding="utf-8")
    return {
        re.sub(r"[^\w\- ]", "", title.lower()).replace(" ", "-")
        for title in re.findall(r"^#{1,6} (.+)$", text, re.M)
    }


def check():
    for name in DOCUMENTS:
        document = ROOT / name
        text = document.read_text(encoding="utf-8")
        links = re.findall(r"\[[^\]]*\]\(([^)]+)\)", text)
        links += re.findall(r'(?:href|src)="([^"]+)"', text)
        for link in links:
            url = urlsplit(link)
            if url.scheme or url.netloc:
                continue
            target = document.parent / unquote(url.path) if url.path else document
            if not target.exists():
                raise ValueError(f"Broken local link in {name}: {link}")
            if url.fragment and target.suffix == ".md" and unquote(url.fragment) not in headings(target):
                raise ValueError(f"Missing heading in {name}: {link}")
    print("Verified local documentation links and heading anchors")


if __name__ == "__main__":
    check()
