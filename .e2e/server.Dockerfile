FROM sandbox-registry.cn-zhangjiakou.cr.aliyuncs.com/opensandbox/server:v0.2.3
RUN /app/.venv/bin/python -m ensurepip && \
    /app/.venv/bin/python -m pip install --no-cache-dir \
      -i https://pypi.tuna.tsinghua.edu.cn/simple \
      'psycopg[binary]==3.3.4' 'psycopg-pool==3.3.1'
COPY opensandbox_server /app/opensandbox_server
