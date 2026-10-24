# Thais for VS Code

A thin client over the Thais WebSocket Agent API.

```sh
cd extensions/vscode
npm install
npm run compile
```

Start the daemon:

```sh
thais daemon --addr 127.0.0.1:8787
```

Use the printed `ws://...?token=...` URL in **Thais: Connect to daemon**.
Commands: new session, send task, switch session, select model, resolve approval.
