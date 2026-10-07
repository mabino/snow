# Builds the "Network Apps" test disk (see make-disk.sh)
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends unar python3 python3-pip ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && pip3 install --no-cache-dir --break-system-packages machfs==1.3
COPY build.py /usr/local/bin/build.py
ENTRYPOINT ["python3", "/usr/local/bin/build.py"]
