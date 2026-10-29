#!/usr/bin/env python3
import argparse
import csv
import gzip
import hashlib
import heapq
import http.client
import re
import shutil
import sys
import tarfile
import urllib.parse
import urllib.request
import zipfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

WOO_DOCUMENTS = "https://ssh.datastations.nl/api/access/datafile/614459"
WOO_DOWNLOAD = "https://doi.wooverheid.nl/?doi={}"
GOVDOCS_THREAD = "https://downloads.digitalcorpora.org/corpora/files/govdocs1/threads/thread{}.zip"
ENRON = "https://www.cs.cmu.edu/~enron/enron_mail_20150507.tar.gz"
ENRON_MESSAGES = 517401
USER_AGENT = "zontologize-fetch-bench-data"

def download(url, target):
    if target.exists():
        return target
    target.parent.mkdir(parents=True, exist_ok=True)
    partial = target.with_name(target.name + ".part")
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    print(f"downloading {url}", file=sys.stderr)
    with urllib.request.urlopen(request, timeout=120) as response, open(partial, "wb") as out:
        shutil.copyfileobj(response, out, 1 << 20)
    partial.rename(target)
    return target


def rank(name, seed=0):
    return int.from_bytes(hashlib.sha1(f"{seed}:{name}".encode()).digest()[:8], "big")


def keep(name, wanted, total):
    return rank(name) < wanted / total * 2**64


def safe(name):
    return re.sub(r"[^\w.-]", "_", name).lstrip(".") or "_"


def woo_extension(row):
    name = urllib.parse.unquote(row["foi_fileName"]).lower()
    extension = name.rsplit(".", 1)[-1] if "." in name else ""
    return extension if extension.isalnum() and len(extension) <= 5 else "bin"


def woo_fetch(row, out):
    identifier = row["dc_identifier"]
    extension = woo_extension(row)
    target = out / "woo" / safe(row["foi_dossierId"]) / f"{safe(identifier)}.{extension}"
    if target.exists():
        return target
    request = urllib.request.Request(WOO_DOWNLOAD.format(identifier), headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            body = response.read()
    except (OSError, http.client.HTTPException) as error:
        print(f"skip {identifier}: {error}", file=sys.stderr)
        return None
    if not body or (body.lstrip()[:1] == b"<" and extension not in ("html", "htm", "xml")):
        print(f"skip {identifier}: no file returned", file=sys.stderr)
        return None
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(body)
    return target


def woo(out, cache, count, seed, jobs):
    documents = download(WOO_DOCUMENTS, cache / "woo_documents.csv.gz")
    csv.field_size_limit(sys.maxsize)
    fields = ("dc_identifier", "foi_dossierId", "foi_fileName")
    with gzip.open(documents, "rt", encoding="utf-8", newline="") as file:
        dutch = (
            {field: row[field] for field in fields}
            for row in csv.DictReader(file)
            if row["dc_identifier"].startswith("nl.")
        )
        rows = heapq.nsmallest(count * 3, dutch, key=lambda row: rank(row["dc_identifier"], seed))
    entries = []
    with ThreadPoolExecutor(jobs) as pool:
        start = 0
        while len(entries) < count and start < len(rows):
            batch = rows[start : start + count - len(entries)]
            start += len(batch)
            for row, target in zip(batch, pool.map(lambda row: woo_fetch(row, out), batch)):
                if target:
                    entries.append((target, "woo", row["dc_identifier"]))
    return entries


def govdocs(out, cache, threads):
    entries = []
    for thread in threads:
        archive = download(GOVDOCS_THREAD.format(thread), cache / f"govdocs-thread{thread}.zip")
        with zipfile.ZipFile(archive) as zipped:
            for member in zipped.infolist():
                if member.is_dir():
                    continue
                target = out / "govdocs" / f"thread{thread}" / safe(Path(member.filename).name)
                if not target.exists():
                    target.parent.mkdir(parents=True, exist_ok=True)
                    with zipped.open(member) as source, open(target, "wb") as dest:
                        shutil.copyfileobj(source, dest)
                entries.append((target, "govdocs", member.filename))
    return entries


def enron(out, cache, count):
    archive = download(ENRON, cache / "enron_mail_20150507.tar.gz")
    entries = []
    with tarfile.open(archive, "r|gz") as tar:
        for member in tar:
            if not member.isfile() or not keep(member.name, count, ENRON_MESSAGES):
                continue
            parts = Path(member.name).parts
            if len(parts) < 3 or ".." in parts:
                continue
            target = out / "enron" / Path(*map(safe, parts[1:-1])) / f"{safe(parts[-1].rstrip('.'))}.eml"
            if not target.exists():
                target.parent.mkdir(parents=True, exist_ok=True)
                with tar.extractfile(member) as source, open(target, "wb") as dest:
                    shutil.copyfileobj(source, dest)
            entries.append((target, "enron", member.name))
    return entries


def main():
    parser = argparse.ArgumentParser(description="Fetch the zontologize benchmark folder from public sources")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--woo", type=int, default=1000, help="number of Woo documents")
    parser.add_argument("--govdocs-threads", type=int, nargs="*", default=[0], help="Govdocs1 threads of 1,000 files")
    parser.add_argument("--enron", type=int, default=1000, help="approximate number of Enron messages")
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--jobs", type=int, default=8)
    args = parser.parse_args()

    out = args.out
    cache = out / ".cache"
    entries = []
    if args.woo:
        entries += woo(out, cache, args.woo, args.seed, args.jobs)
    entries += govdocs(out, cache, args.govdocs_threads)
    if args.enron:
        entries += enron(out, cache, args.enron)

    with open(out / "manifest.csv", "w", newline="", encoding="utf-8") as file:
        writer = csv.writer(file)
        writer.writerow(["path", "source", "identifier"])
        for target, source, identifier in sorted(entries):
            writer.writerow([target.relative_to(out).as_posix(), source, identifier])
    print(f"{len(entries)} files in {out}", file=sys.stderr)


if __name__ == "__main__":
    main()
