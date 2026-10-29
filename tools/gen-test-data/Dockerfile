FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libreoffice-writer-nogui libreoffice-calc-nogui \
        poppler-utils imagemagick ghostscript python3 \
        fonts-dejavu fonts-liberation2 fonts-crosextra-carlito \
    && rm -rf /var/lib/apt/lists/*
RUN sed -i 's#<policy domain="coder" rights="none" pattern="PDF" />#<policy domain="coder" rights="read|write" pattern="PDF" />#' /etc/ImageMagick-*/policy.xml || true
WORKDIR /work
ENTRYPOINT ["python3", "/gen/gen.py"]
