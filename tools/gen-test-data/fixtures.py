import argparse
import subprocess
import tempfile
from pathlib import Path

BUNDLE = [
    [
        "Certificate of conformity",
        "Certificate no. C-1001",
        "Project P-2021-0042",
        "Gear pump GP-40 tested and accepted.",
    ],
    [
        "Annex: measurements",
        "Flow 12.5 m3/h at 6 bar, no leaks found.",
    ],
    [
        "Certificate of conformity",
        "Certificate no. C-1002",
        "Project P-2022-0107",
        "Ball valve BV-25 tested and accepted.",
    ],
]

LETTER = [
    [
        "Brief aan Northwind Pumps",
        "Betreft: project P-2O2l-OO42",
        "De nieuwe pomp is geplaatst en getest.",
    ],
]


def escape(text):
    return text.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


def text_pdf(pages, size=14):
    objects = ["<< /Type /Catalog /Pages 2 0 R >>", None,
               "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"]
    kids = []
    for lines in pages:
        body = ["BT", f"/F1 {size} Tf", f"{size + 6} TL", "72 760 Td"]
        for line in lines:
            body.append(f"({escape(line)}) Tj T*")
        body.append("ET")
        stream = "\n".join(body)
        objects.append(f"<< /Length {len(stream)} >>\nstream\n{stream}\nendstream")
        content = len(objects)
        objects.append(f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] "
                       f"/Resources << /Font << /F1 3 0 R >> >> /Contents {content} 0 R >>")
        kids.append(f"{len(objects)} 0 R")
    objects[1] = f"<< /Type /Pages /Kids [{' '.join(kids)}] /Count {len(kids)} >>"
    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for number, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{number} 0 obj\n{body}\nendobj\n".encode("latin-1")
    xref = len(out)
    out += f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n".encode()
    for offset in offsets:
        out += f"{offset:010d} 00000 n \n".encode()
    out += f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    return bytes(out)


def scan_pdf(pages, target):
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        source = work / "source.pdf"
        source.write_bytes(text_pdf(pages, size=18))
        subprocess.run(["pdftoppm", "-r", "200", "-gray", "-png", str(source), str(work / "page")],
                       check=True)
        images = sorted(str(p) for p in work.glob("page*.png"))
        subprocess.run(["magick", *images, "-compress", "jpeg", "-quality", "85", str(target)],
                       check=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    folder = Path(args.out)
    folder.mkdir(parents=True, exist_ok=True)
    (folder / "Certificates 0003.pdf").write_bytes(text_pdf(BUNDLE))
    scan_pdf(LETTER, folder / "Brief 0004.pdf")


if __name__ == "__main__":
    main()
