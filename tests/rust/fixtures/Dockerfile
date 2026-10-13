# Copyright 2026 The OpenSandbox Authors
# SPDX-License-Identifier: Apache-2.0
FROM python:3.11-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends curl util-linux bash procps \
    && rm -rf /var/lib/apt/lists/* \
    && pip install --no-cache-dir jupyter-server==2.17.0 ipykernel==6.30.1 \
    && python -m ipykernel install --prefix=/usr/local --name python --display-name Python
ENV JUPYTER_HOST=http://127.0.0.1:44771 JUPYTER_TOKEN=rust-e2e-jupyter-safe-token
CMD ["jupyter", "server", "--ip=0.0.0.0", "--port=44771", "--allow-root", "--no-browser", "--IdentityProvider.token=rust-e2e-jupyter-safe-token"]
