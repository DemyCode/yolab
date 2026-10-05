FROM ollama/ollama:0.35.1@sha256:292ee7945dfc3d5840a181f3ab86fedb1e66703e02c8af98b50f4da56b7e278c
RUN apt-get update \
    && apt-get install -y --no-install-recommends mesa-vulkan-drivers=25.2.8-0ubuntu0.24.04.4 \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*
ENV OLLAMA_VULKAN=1
