import * as vscode from "vscode";
import WebSocket from "ws";

type Rpc = { id: number; method: string; params?: unknown };

class ThaisClient {
  private socket?: WebSocket;
  private nextId = 1;
  private pending = new Map<number, (value: any) => void>();
  private output = vscode.window.createOutputChannel("Thais");
  private sessionId?: string;

  async connect(): Promise<void> {
    const url = await vscode.window.showInputBox({
      prompt: "WebSocket URL",
      value: "ws://127.0.0.1:8787/?token=",
    });
    if (!url) return;
    await new Promise<void>((resolve, reject) => {
      const socket = new WebSocket(url);
      socket.once("open", () => {
        this.socket = socket;
        this.output.show(true);
        this.output.appendLine(`connected to ${url}`);
        resolve();
      });
      socket.once("error", reject);
      socket.on("message", (raw) => this.onMessage(raw.toString()));
    });
  }

  private onMessage(text: string) {
    const message = JSON.parse(text);
    if (typeof message.id === "number") {
      const resolve = this.pending.get(message.id);
      this.pending.delete(message.id);
      resolve?.(message);
      return;
    }
    if (message.method !== "session.event") return;
    const event = message.params?.event;
    if (!event) return;
    if (event.kind === "model_delta") this.output.append(event.payload.text ?? "");
    if (event.kind === "turn_completed") this.output.appendLine("\n");
    if (event.kind === "error") this.output.appendLine(`\nerror: ${event.payload.message}`);
    if (event.kind === "permission_requested") {
      this.handleApproval(event);
    }
  }

  async request(method: string, params: unknown): Promise<any> {
    if (!this.socket) throw new Error("not connected");
    const id = this.nextId++;
    const message: Rpc = { id, method, params };
    return new Promise((resolve) => {
      this.pending.set(id, resolve);
      this.socket!.send(JSON.stringify({ jsonrpc: "2.0", ...message }));
    });
  }

  async newSession() {
    const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (!folder) throw new Error("open a workspace first");
    const result = await this.request("session.create", { workspace: folder, mode: "build" });
    this.sessionId = result.result.session.session_id;
    await this.request("session.subscribe", { session_id: this.sessionId, from_seq: 0 });
    this.output.appendLine(`session ${this.sessionId}`);
  }

  async send() {
    if (!this.sessionId) await this.newSession();
    const task = await vscode.window.showInputBox({ prompt: "Task" });
    if (!task) return;
    await this.request("session.send", { session_id: this.sessionId, input: task });
  }

  async sessions() {
    const result = await this.request("session.list", { include_archived: true });
    const sessions = result.result.sessions as any[];
    const picked = await vscode.window.showQuickPick(
      sessions.map((session) => ({
        label: session.name,
        description: `${session.mode} · ${session.status} · ${session.model}`,
        session,
      })),
      { placeHolder: "Select a session" }
    );
    if (!picked) return;
    await this.request("session.resume", { session_id: picked.session.session_id });
    await this.request("session.subscribe", { session_id: picked.session.session_id, from_seq: 0 });
    this.sessionId = picked.session.session_id;
  }

  async models() {
    const result = await this.request("models.list", {});
    const models = result.result.models as any[];
    const picked = await vscode.window.showQuickPick(
      models.map((model) => ({ label: model.id, description: model.provider })),
      { placeHolder: "Select the default model" }
    );
    if (!picked || !this.sessionId) return;
    await this.request("session.set_model", {
      session_id: this.sessionId,
      role: "default",
      model: picked.label,
    });
  }

  private async handleApproval(event: any) {
    const action = event.payload.action ?? {};
    const choice = await vscode.window.showWarningMessage(
      `Approve ${action.tool ?? action.type ?? "action"}?`,
      { modal: true },
      "Allow",
      "Deny"
    );
    if (!choice) return;
    await this.request("session.approve", {
      session_id: event.session_id,
      approval_id: event.payload.approval_id,
      decision: choice === "Allow" ? "allow" : "deny",
    });
  }
}

const client = new ThaisClient();

export function activate(context: vscode.ExtensionContext) {
  context.subscriptions.push(
    vscode.commands.registerCommand("thais.connect", () => client.connect()),
    vscode.commands.registerCommand("thais.newSession", () => client.newSession()),
    vscode.commands.registerCommand("thais.send", () => client.send()),
    vscode.commands.registerCommand("thais.sessions", () => client.sessions()),
    vscode.commands.registerCommand("thais.models", () => client.models())
  );
}

export function deactivate() {}
