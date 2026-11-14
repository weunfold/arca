# The image an Arca function runs the nightly analysis from. Node with nothing installed: the analysis has no
# dependency. Pinned by digest (node:22-alpine, read 2026-11-14); a new base is a new digest here and a new function.
FROM docker.io/library/node:22-alpine@sha256:1314fb30ec37592d5845e5a160d522c0e680972aa534549940b8d73bde88481a
WORKDIR /app
COPY package.json ./
COPY src ./src
USER node
# A function names its command as an absolute path: ["/usr/local/bin/node", "/app/src/cli.js"].
ENTRYPOINT ["/usr/local/bin/node", "/app/src/cli.js"]
