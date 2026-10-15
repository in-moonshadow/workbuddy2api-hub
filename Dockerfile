# WorkBuddy Multi-Account Reverse Proxy Gateway
FROM python:3.11-alpine

# === Build-time proxy (construction only; no daemon-level / runtime effect) ===
# 仅在构建期（拉取 Alpine 包等 build 阶段）允许走代理；运行时容器网络保持不代理。
# 用法：docker build --build-arg HTTP_PROXY=$HTTP_PROXY --build-arg HTTPS_PROXY=$HTTPS_PROXY .
# 不传则留空 == 不走代理，行为与老版本一致；切勿把代理写进 runtime env / daemon.json。
ARG HTTP_PROXY=""
ARG HTTPS_PROXY=""
ARG NO_PROXY="localhost,127.0.0.1"

# Runtime env does NOT include any proxy variables — proxy only for build stage below.
ENV PYTHONUNBUFFERED=1     HOST=0.0.0.0     PORT=8788     API_KEY=     TZ=Asia/Shanghai

WORKDIR /app

# Alpine timezone & certs (build-time proxy only)
RUN HTTP_PROXY="${HTTP_PROXY}" HTTPS_PROXY="${HTTPS_PROXY}" NO_PROXY="${NO_PROXY}"       apk add --no-cache tzdata ca-certificates &&     cp /usr/share/zoneinfo/${TZ} /etc/localtime &&     echo "${TZ}" > /etc/timezone

# Copy application files (Zero external pip dependencies needed)
COPY wb_*.py dashboard.html ./

# Create data directories
RUN mkdir -p /app/accounts /app/usage

# Volume persistence for credentials and usage logs
VOLUME ["/app/accounts", "/app/usage"]

EXPOSE 8788

# Health probe for docker / orchestrators: /health always answers 200 and
# only exposes account identity to an authorised caller. The URL follows
# PORT so a remapped container still reports healthy.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD python -c "import os,urllib.request; urllib.request.urlopen('http://127.0.0.1:%s/health' % os.environ.get('PORT','8788'), timeout=3)" || exit 1

# Launch the proxy in the project's LAN mode: --lan listens on every interface
# and forces an api key (generated once, persisted in ./accounts/settings.json
# and printed in the startup log). Without it the container published port 8788
# to the network while key checking stayed off.
# No --port on purpose: the gateway already reads PORT from the environment
# (default 8788), and the exec form cannot expand a variable. Keeping the exec
# form leaves python as PID 1, so `docker stop` still delivers SIGTERM.
CMD ["python", "wb_proxy.py", "--host", "0.0.0.0", "--lan"]
