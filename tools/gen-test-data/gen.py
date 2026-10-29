#!/usr/bin/env python3
import argparse
import csv
import datetime
import email.message
import email.utils
import html
import random
import shutil
import subprocess
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

KINDS = {
    "pdf_text": 200,
    "pdf_scan": 120,
    "docx": 120,
    "doc": 60,
    "xlsx": 80,
    "xls": 40,
    "eml": 120,
    "html": 80,
    "txt": 80,
    "csv": 60,
    "jpg": 20,
    "png": 20,
}

EXTENSIONS = {"pdf_text": "pdf", "pdf_scan": "pdf"}

FOLDERS = {
    "nl": {
        "root": "Projecten",
        "pdf_text": "04 Rapporten",
        "pdf_scan": "03 Tekeningen",
        "docx": "01 Correspondentie",
        "doc": "04 Rapporten",
        "xlsx": "02 Offertes",
        "xls": "05 Facturen",
        "eml": "01 Correspondentie",
        "html": "06 Documentatie",
        "txt": "06 Documentatie",
        "csv": "05 Facturen",
        "jpg": "03 Tekeningen",
        "png": "03 Tekeningen",
    },
    "en": {
        "root": "Projects",
        "pdf_text": "04 Reports",
        "pdf_scan": "03 Drawings",
        "docx": "01 Correspondence",
        "doc": "04 Reports",
        "xlsx": "02 Quotations",
        "xls": "05 Invoices",
        "eml": "01 Correspondence",
        "html": "06 Documentation",
        "txt": "06 Documentation",
        "csv": "05 Invoices",
        "jpg": "03 Drawings",
        "png": "03 Drawings",
    },
}

ARCHIVE = "Archief/Scans"

SYLLABLES = [
    "bra", "kel", "dor", "vin", "mar", "tos", "lin", "gru", "hav", "pel",
    "sto", "rek", "wan", "zil", "fra", "nok", "tem", "bur", "qui", "las",
    "dre", "vos", "mun", "ter", "jol", "kan", "sip", "rov", "ell", "gab",
]

FIRST_NAMES = [
    "Anna", "Bram", "Carla", "Daan", "Eva", "Femke", "Gijs", "Hanna", "Ivo",
    "Joris", "Kim", "Lotte", "Milan", "Noor", "Olaf", "Pim", "Roos", "Sven",
    "Tess", "Ruben", "Sara", "Tom", "Vera", "Wout", "Yara", "Lena", "Mark",
]

COMPANY_FORMS = {"nl": ["BV", "Techniek BV", "Installaties BV", "Groep"],
                 "en": ["Ltd", "Engineering Ltd", "Systems", "Industries"]}

ITEMS = {
    "nl": ["pomp", "afsluiter", "regelkast", "drukopnemer", "filterhuis",
           "leidingdeel", "warmtewisselaar", "ventilator", "motor", "frequentieregelaar",
           "terugslagklep", "koeler", "tank", "flens", "manometer"],
    "en": ["pump", "valve", "control panel", "pressure sensor", "filter housing",
           "pipe section", "heat exchanger", "fan", "motor", "frequency drive",
           "check valve", "cooler", "tank", "flange", "pressure gauge"],
}

TITLES = {
    "pdf_text": {"nl": "Inspectierapport", "en": "Inspection report"},
    "pdf_scan": {"nl": "Werkbon", "en": "Work order"},
    "docx": {"nl": "Brief", "en": "Letter"},
    "doc": {"nl": "Onderhoudsrapport", "en": "Maintenance report"},
    "xlsx": {"nl": "Offerte", "en": "Quotation"},
    "xls": {"nl": "Factuur", "en": "Invoice"},
    "eml": {"nl": "Bericht", "en": "Message"},
    "html": {"nl": "Handleiding", "en": "Manual"},
    "txt": {"nl": "Notitie", "en": "Note"},
    "csv": {"nl": "Factuurregels", "en": "Invoice lines"},
    "jpg": {"nl": "Typeplaat", "en": "Name plate"},
    "png": {"nl": "Typeplaat", "en": "Name plate"},
}

SENTENCES = {
    "nl": [
        "Op {date} is de {item} op locatie {site} gecontroleerd door {person}.",
        "De {item} van project {project} vertoont lichte slijtage en wordt binnen {n} weken vervangen.",
        "Wij bevestigen de ontvangst van uw bestelling {order} voor {client}.",
        "De gemeten druk bedroeg {value} bar bij een temperatuur van {temp} graden Celsius.",
        "Volgens tekening {drawing} moet de {item} aan de noordzijde worden geplaatst.",
        "Het totaalbedrag van {amount} euro is exclusief btw en geldig tot {date}.",
        "Na overleg met {person} is besloten de werkzaamheden uit te stellen tot week {n}.",
        "Bij de oplevering zijn {n} punten genoteerd die nog moeten worden afgehandeld.",
        "De {item} is getest volgens procedure {order} en voldoet aan de eisen.",
        "Graag ontvangen wij uw reactie voor {date} zodat de planning rond kan.",
        "Tijdens het bezoek aan {site} bleek de {item} niet goed bereikbaar te zijn.",
        "Het onderhoudsinterval voor de {item} wordt verlengd naar {n} maanden.",
    ],
    "en": [
        "On {date} the {item} at site {site} was inspected by {person}.",
        "The {item} of project {project} shows light wear and will be replaced within {n} weeks.",
        "We confirm receipt of your purchase order {order} for {client}.",
        "The measured pressure was {value} bar at a temperature of {temp} degrees Celsius.",
        "According to drawing {drawing} the {item} must be placed on the north side.",
        "The total amount of {amount} euro excludes VAT and is valid until {date}.",
        "After consulting {person} it was decided to postpone the work until week {n}.",
        "At handover {n} open points were recorded that still need to be resolved.",
        "The {item} was tested according to procedure {order} and meets the requirements.",
        "Please reply before {date} so that the schedule can be finalised.",
        "During the visit to {site} the {item} turned out to be hard to reach.",
        "The maintenance interval for the {item} is extended to {n} months.",
    ],
}

TABLE_HEADERS = {
    "nl": ["Regel", "Omschrijving", "Aantal", "Prijs", "Bedrag"],
    "en": ["Line", "Description", "Quantity", "Price", "Amount"],
}


def word(rng, parts=2):
    return "".join(rng.choice(SYLLABLES) for _ in range(parts)).capitalize()


class World:
    def __init__(self, rng):
        self.rng = rng
        self.clients = []
        for lang in ("nl", "en"):
            for _ in range(12):
                name = f"{word(rng, rng.randint(2, 3))} {rng.choice(COMPANY_FORMS[lang])}"
                self.clients.append((lang, name))
        self.sites = [word(rng, 3) for _ in range(30)]
        self.people = [f"{rng.choice(FIRST_NAMES)} {word(rng, 2)}" for _ in range(60)]
        self.projects = []
        for i in range(60):
            lang, client = rng.choice(self.clients)
            year = rng.randint(2016, 2025)
            code = f"P-{year}-{rng.randint(1, 999):04d}"
            self.projects.append({
                "lang": lang, "client": client, "year": year, "code": code,
                "site": rng.choice(self.sites), "index": i,
            })


def fill(rng, world, project, template):
    date = datetime.date(project["year"], 1, 1) + datetime.timedelta(days=rng.randint(0, 364))
    return template.format(
        date=date.strftime("%d-%m-%Y"),
        item=rng.choice(ITEMS[project["lang"]]),
        site=project["site"],
        person=rng.choice(world.people),
        project=project["code"],
        client=project["client"],
        order=f"PO-{rng.randint(10000, 99999)}",
        drawing=f"DWG-{rng.randint(100, 999)}-{rng.choice('ABCD')}{rng.randint(1, 9)}",
        value=f"{rng.uniform(1, 40):.1f}".replace(".", "," if project["lang"] == "nl" else "."),
        temp=rng.randint(5, 95),
        amount=f"{rng.randint(100, 250000):,}".replace(",", "."),
        n=rng.randint(2, 52),
    )


def make_doc(rng, world, project, kind, pages):
    lang = project["lang"]
    title = f"{TITLES[kind][lang]} {project['code']} {project['client']}"
    paragraphs = []
    for _ in range(pages * rng.randint(5, 7)):
        sentences = [fill(rng, world, project, rng.choice(SENTENCES[lang]))
                     for _ in range(rng.randint(3, 6))]
        paragraphs.append(" ".join(sentences))
    rows = []
    for line in range(1, rng.randint(4, 25)):
        qty = rng.randint(1, 20)
        price = rng.randint(15, 9000)
        rows.append([str(line), rng.choice(ITEMS[lang]).capitalize(), str(qty),
                     str(price), str(qty * price)])
    return {"title": title, "lang": lang, "paragraphs": paragraphs, "rows": rows,
            "header": TABLE_HEADERS[lang]}


def to_html(doc):
    parts = [f"<!DOCTYPE html>\n<html lang=\"{doc['lang']}\"><head><meta charset=\"utf-8\">"
             f"<title>{html.escape(doc['title'])}</title></head><body>",
             f"<h1>{html.escape(doc['title'])}</h1>"]
    for i, paragraph in enumerate(doc["paragraphs"]):
        if i and i % 6 == 0:
            parts.append(f"<h2>{i // 6}.</h2>")
        parts.append(f"<p>{html.escape(paragraph)}</p>")
    parts.append("<table border=\"1\"><tr>"
                 + "".join(f"<th>{html.escape(h)}</th>" for h in doc["header"]) + "</tr>")
    for row in doc["rows"]:
        parts.append("<tr>" + "".join(f"<td>{html.escape(c)}</td>" for c in row) + "</tr>")
    parts.append("</table></body></html>\n")
    return "\n".join(parts)


def to_text(doc):
    lines = [doc["title"], ""]
    for paragraph in doc["paragraphs"]:
        lines += [paragraph, ""]
    return "\n".join(lines)


def write_csv(path, doc):
    with open(path, "w", newline="", encoding="utf-8") as f:
        writer = csv.writer(f, delimiter=";")
        writer.writerow(doc["header"])
        writer.writerows(doc["rows"])


def write_eml(path, rng, world, project, doc, index):
    message = email.message.EmailMessage()
    sender = rng.choice(world.people)
    to = rng.choice(world.people)
    domain = project["client"].split()[0].lower() + ".example"
    message["From"] = f"{sender} <{sender.split()[0].lower()}@{domain}>"
    message["To"] = f"{to} <{to.split()[0].lower()}@example.org>"
    message["Subject"] = doc["title"]
    stamp = datetime.datetime(project["year"], 1, 1, 8, tzinfo=datetime.timezone.utc)
    stamp += datetime.timedelta(minutes=rng.randint(0, 525000))
    message["Date"] = email.utils.format_datetime(stamp)
    message["Message-ID"] = f"<{index}.{project['code']}@{domain}>"
    message.set_content(to_text(doc))
    message.add_alternative(to_html(doc), subtype="html")
    if rng.random() < 0.3:
        rows = "\n".join(";".join(r) for r in [doc["header"], *doc["rows"]])
        message.add_attachment(rows.encode(), maintype="text", subtype="csv",
                               filename=f"{project['code']}.csv")
    random.seed(f"eml-{index}-{project['code']}")
    path.write_bytes(bytes(message))


def run(args, cwd=None):
    subprocess.run(args, check=True, cwd=cwd, stdout=subprocess.DEVNULL,
                   stderr=subprocess.DEVNULL)


def soffice(sources, target, outdir, infilter):
    if not sources:
        return
    profile = outdir.parent / "profile"
    outdir.mkdir(parents=True, exist_ok=True)
    for start in range(0, len(sources), 50):
        run(["soffice", f"-env:UserInstallation={profile.as_uri()}", "--headless",
             f"--infilter={infilter}", "--convert-to", target,
             "--outdir", str(outdir), *map(str, sources[start:start + 50])])


def pdf_pages(path):
    out = subprocess.run(["pdfinfo", str(path)], check=True, capture_output=True, text=True)
    for line in out.stdout.splitlines():
        if line.startswith("Pages:"):
            return int(line.split()[1])
    return 0


def degrade(source, target, rng, dpi):
    angle = rng.uniform(-2.0, 2.0)
    blur = rng.uniform(0.3, 1.0)
    noise = rng.uniform(0.2, 0.6)
    run(["magick", "-seed", str(rng.randint(1, 2**31 - 1)), str(source),
         "-colorspace", "Gray", "-background", "white",
         "-rotate", f"{angle:.2f}", "-blur", f"0x{blur:.2f}",
         "-attenuate", f"{noise:.2f}", "+noise", "Gaussian",
         "-level", "8%,92%", "-density", str(dpi), "-units", "PixelsPerInch",
         "-quality", "75", "-strip", str(target)])


def make_scan(source_pdf, target, rng, dpi, image_kind):
    work = Path(tempfile.mkdtemp(dir=target.parent))
    try:
        if image_kind:
            run(["pdftoppm", "-r", str(dpi), "-gray", "-png", "-f", "1", "-l", "1",
                 str(source_pdf), str(work / "page")])
        else:
            run(["pdftoppm", "-r", str(dpi), "-gray", "-png", str(source_pdf), str(work / "page")])
        pages = sorted(work.glob("page*.png"))
        if image_kind:
            degrade(pages[0], target, rng, dpi)
            return 1
        degraded = []
        for page in pages:
            out = page.with_suffix(".jpg")
            degrade(page, out, rng, dpi)
            degraded.append(str(out))
        run(["magick", *degraded, "-compress", "jpeg", "-quality", "75",
             "-density", str(dpi), "-units", "PixelsPerInch", str(target)])
        return len(pages)
    finally:
        shutil.rmtree(work)


def truth_text(source_pdf, image_kind):
    args = ["pdftotext", "-enc", "UTF-8"]
    if image_kind:
        args += ["-f", "1", "-l", "1"]
    out = subprocess.run([*args, str(source_pdf), "-"], check=True, capture_output=True)
    return out.stdout.decode("utf-8")


def plan(rng, world):
    items = []
    for kind, count in KINDS.items():
        for _ in range(count):
            items.append(kind)
    rng.shuffle(items)
    plans = []
    for index, kind in enumerate(items):
        project = rng.choice(world.projects)
        lang = project["lang"]
        folders = FOLDERS[lang]
        if kind == "pdf_scan" and rng.random() < 0.4:
            folder = Path(ARCHIVE) / str(project["year"])
        else:
            project_dir = f"{project['code']} {project['client']} - {project['site']}"
            folder = Path(folders["root"]) / str(project["year"]) / project_dir / folders[kind]
        ext = EXTENSIONS.get(kind, kind)
        name = f"{TITLES[kind][lang]} {project['code']} {index:04d}.{ext}"
        pages = rng.randint(1, 8) if kind == "pdf_scan" else rng.randint(1, 6)
        if kind in ("jpg", "png"):
            pages = 1
        plans.append({"index": index, "kind": kind, "path": folder / name, "project": project,
                      "pages": pages})
    return plans


def main():
    parser = argparse.ArgumentParser(description="Generate the zontologize test folder")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--seed", type=int, default=20261029)
    parser.add_argument("--scale", type=float, default=1.0)
    parser.add_argument("--dpi", type=int, default=300)
    parser.add_argument("--jobs", type=int, default=8)
    args = parser.parse_args()

    for kind in KINDS:
        KINDS[kind] = max(1, round(KINDS[kind] * args.scale))
    rng = random.Random(args.seed)
    world = World(rng)
    plans = plan(rng, world)
    out = args.out
    out.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix="gen-", dir=out.parent))
    manifest = []

    try:
        writer_sources = {"pdf_text": [], "pdf_scan": [], "docx": [], "doc": [],
                          "jpg": [], "png": []}
        calc_sources = {"xlsx": [], "xls": []}
        for item in plans:
            doc_rng = random.Random(f"{args.seed}-{item['index']}")
            doc = make_doc(doc_rng, world, item["project"], item["kind"], item["pages"])
            target = out / item["path"]
            target.parent.mkdir(parents=True, exist_ok=True)
            item["target"] = target
            kind = item["kind"]
            if kind == "txt":
                target.write_text(to_text(doc), encoding="utf-8")
            elif kind == "html":
                target.write_text(to_html(doc), encoding="utf-8")
            elif kind == "csv":
                write_csv(target, doc)
            elif kind == "eml":
                write_eml(target, doc_rng, world, item["project"], doc, item["index"])
            elif kind in writer_sources:
                source = stage / "src" / kind / f"{item['index']:04d}.html"
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_text(to_html(doc), encoding="utf-8")
                writer_sources[kind].append(source)
            else:
                source = stage / "src" / kind / f"{item['index']:04d}.csv"
                source.parent.mkdir(parents=True, exist_ok=True)
                write_csv(source, doc)
                calc_sources[kind].append(source)

        html_in = "HTML (StarWriter)"
        csv_in = "CSV:59,34,76,1"
        pdf_like = writer_sources["pdf_text"] + writer_sources["pdf_scan"] \
            + writer_sources["jpg"] + writer_sources["png"]
        soffice(pdf_like, "pdf", stage / "pdf", html_in)
        soffice(writer_sources["docx"], "docx:MS Word 2007 XML", stage / "docx", html_in)
        soffice(writer_sources["doc"], "doc:MS Word 97", stage / "doc", html_in)
        soffice(calc_sources["xlsx"], "xlsx:Calc MS Excel 2007 XML", stage / "xlsx", csv_in)
        soffice(calc_sources["xls"], "xls:MS Excel 97", stage / "xls", csv_in)

        def finish(item):
            kind = item["kind"]
            target = item["target"]
            stem = f"{item['index']:04d}"
            row = {"path": item["path"].as_posix(), "kind": kind, "pages": "",
                   "has_text_layer": "", "truth_file": ""}
            if kind in ("txt", "html", "csv", "eml"):
                return row
            if kind in ("docx", "doc", "xlsx", "xls"):
                shutil.move(stage / kind / f"{stem}.{kind}", target)
                return row
            source_pdf = stage / "pdf" / f"{stem}.pdf"
            if kind == "pdf_text":
                shutil.move(source_pdf, target)
                row.update(pages=pdf_pages(target), has_text_layer="true")
                return row
            scan_rng = random.Random(f"{args.seed}-scan-{item['index']}")
            image_kind = kind in ("jpg", "png")
            pages = make_scan(source_pdf, target, scan_rng, args.dpi, image_kind)
            truth = target.with_name(target.name + ".truth.txt")
            truth.write_text(truth_text(source_pdf, image_kind), encoding="utf-8")
            row.update(pages=pages, has_text_layer="false",
                       truth_file=truth.relative_to(out).as_posix())
            return row

        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            manifest = list(pool.map(finish, plans))
    finally:
        shutil.rmtree(stage, ignore_errors=True)

    with open(out / "manifest.csv", "w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=["path", "kind", "pages", "has_text_layer",
                                               "truth_file"])
        writer.writeheader()
        writer.writerows(manifest)
    print(f"wrote {len(manifest)} files to {out}")


if __name__ == "__main__":
    main()
