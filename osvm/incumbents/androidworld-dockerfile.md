# AndroidWorld @55ddaf5 Dockerfile: two changes needed to build it in 2026-11 (applied identically for every run)

1. Base image: `openjdk:18-jdk-slim` was removed from Docker Hub (deprecated official image).
   Replaced with `eclipse-temurin:17-jdk-jammy` (Ubuntu 22.04 + JDK 17; sdkmanager runs on 17).
2. `COPY . /` then `uv pip install . --system` makes `/` the project root: setuptools' egg_info file scan walks
   all of `/sys` and `/proc` (following links). On the 96-vCPU metal host it ran 38 CPU-minutes without finishing.
   Replaced with `WORKDIR /aw` + `COPY . /aw` (the entrypoint and server use relative paths, so nothing else changes).

Command (on the host, in the android_world checkout):
    sed -e "s#^FROM openjdk:18-jdk-slim#FROM eclipse-temurin:17-jdk-jammy#" -e "s#^COPY \. /\$#WORKDIR /aw\nCOPY . /aw#" Dockerfile > Dockerfile.t5
    docker build -f Dockerfile.t5 -t android_world:55ddaf5 .
3. The image's server fails at import: `ModuleNotFoundError: No module named 'PIL'` (pyproject.toml, used only by
   the Docker path, does not list Pillow). Added `&& uv pip install --system pillow` to the install line (version
   resolved at build time is recorded in raw/MANIFEST.json).

Final command:
    sed -e "s#^FROM openjdk:18-jdk-slim#FROM eclipse-temurin:17-jdk-jammy#" -e "s#^COPY \. /\$#WORKDIR /aw\nCOPY . /aw#" \
        -e "s#^RUN uv pip install \. --system\$#RUN uv pip install . --system \&\& uv pip install --system pillow#" Dockerfile > Dockerfile.t5
