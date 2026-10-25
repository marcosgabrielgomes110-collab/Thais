<p align="center">
  <img src="Pictures/Thais.png" alt="Thais" width="300" />
</p>

<h1 align="center">Thais</h1>

<p align="center">
  <strong>Seu agente de código local-first.</strong><br/>
  Planeja antes de editar, pede permissão antes de agir e lembra o que importa.
</p>

<p align="center">
  <a href="https://github.com/marcosgabrielgomes110-collab/Thais/actions/workflows/ci.yml">
    <img src="https://img.shields.io/badge/CI-passing-2f6f5e?logo=githubactions&logoColor=white" alt="CI passing" />
  </a>
  <img src="https://img.shields.io/badge/rust-1.99.1-b7410e?logo=rust&logoColor=white" alt="Rust 1.99.1" />
  <img src="https://img.shields.io/badge/local--first-yes-2f6f5e" alt="Local-first" />
  <img src="https://img.shields.io/badge/telemetry-none-2f6f5e" alt="No telemetry" />
  <img src="https://img.shields.io/badge/status-Linux%20v1-6f42c1" alt="Linux v1" />
</p>

---

Thais é um agente de código que roda na sua máquina, no seu repositório, com as suas regras.
Ele combina um loop de trabalho explícito com um gate de permissões, snapshots reversíveis,
sessões persistentes e memória de projeto aprovada por você.

Sem nuvem obrigatória. Sem telemetria. Sem ações silenciosas.

## Por que Thais

**Planejar é um estado, não uma sugestão.** Em plan mode, Thais só lê. A passagem para build
acontece quando você aprova o plano.

**Toda ação perigosa passa pelo gate.** Tool calls, subagentes e escritas de memória são
classificados como `allow`, `ask` ou `deny`. `bash` continua sempre em `ask` até o sandbox
de sistema operacional chegar.

**Mudanças são reversíveis.** Antes de uma mutação, o runtime cria um snapshot shadow-Git.
Falha de ferramenta, verificação ou LSP restaura o estado anterior.

**Sessões não são descartáveis.** Cada sessão tem checkpoint, event stream, modelo, modo,
orçamento, histórico e lease de workspace. Você pode retomar, arquivar ou apagar.

**O contexto não estoura em silêncio.** Quando o orçamento de contexto aperta, Thais escreve
um checkpoint antes, resume turnos antigos, preserva os recentes e continua.

**Seu modelo, suas regras.** OpenAI, Anthropic, Cohere, OpenRouter, DeepSeek e presets locais
compartilham o mesmo loop. Roles e fallback ficam visíveis no log.

## O que já tem

| Área | Hoje |
|---|---|
| **Sessões** | criação, lista, resume, rename, archive, soft-delete, checkpoint e event stream |
| **Workspace** | lease SQLite: uma sessão mutável por workspace, várias read-only, TTL e steal explícito |
| **Modos** | plan read-only, build com aprovação e transição explícita plan → build |
| **Ferramentas** | leitura, grep/glob, delete/move/mkdir, memória, escrita, edit, apply_patch, bash/jobs, test/build, git status/diff/log/show/branch/checkout/stash/commit e secret_scan |
| **Segurança** | permission gate, path jail, preview de diff, snapshots, rollback e reparo limitado |
| **Verificação** | comandos do usuário, diagnósticos LSP e retorno `pass` / `fail` / `unavailable` |
| **Memória** | `MEMORY.md` com escrita aprovada, diff, cap de tokens, duplicatas e precedência abaixo de `AGENTS.md` |
| **Continuidade** | checkpoint write-ahead, tarefas derivadas do plano, compactação e rebuild de contexto |
| **Histórico** | `history.search` com FTS5, filtros por sessão/workspace e índice reconstruível |
| **Modelos** | registry de providers, catálogo de modelos, roles, fallback, override por turno e custo |
| **Provedores** | OpenAI, Anthropic nativo, Cohere, OpenRouter, DeepSeek, Qwen, Z.ai, Ollama, vLLM, LiteLLM |
| **Ferramentas externas** | LSP para diagnósticos/definições/referências e MCP via stdio ou HTTP |
| **Subagentes** | `explore` read-only e `general` com gate, verificação, rollback e profundidade 1 |
| **Interfaces** | CLI, TUI, Agent API JSON-RPC, SDK tipado e testes de contrato |
| **Durabilidade** | SQLite WAL + FTS5, Event Log append-only e redaction antes da persistência |

## Arquitetura

```mermaid
flowchart TD
    U[User<br/><i>terminal or IDE</i>]
    TUI[TUI and CLI<br/><i>stream, plan review, approvals, cancel</i>]

    subgraph CORE["Core"]
        O[Thais<br/><i>main agent: state, modes, budgets</i>]
        P([Plan Approval<br/><i>state: plan → build</i>])
        C[Context Manager<br/><i>assembles the prompt</i>]
        L["Model (LLM)<br/><i>proposes the next step</i>]
        O <--> C
        C <--> L
        C -.->|token threshold| O
        O -->|plan ready| P
        P -->|approved: build mode| O
        P -->|rejected: plan mode| O
    end

    subgraph EDGE["API boundary"]
        API[Agent API<br/><i>session routing, stdio / WebSocket</i>]
    end

    subgraph ACT["Control and action"]
        G[Permission Gate<br/><i>mode + policy: allow, ask, deny</i>]
        T[Tools<br/><i>fs, bash, git, LSP, MCP</i>]
        S[Subagents<br/><i>bounded, inherit mode and budget</i>]
        V[Verifier<br/><i>tests, lint, LSP after code change</i>]
        R[Rollback<br/><i>snapshot, Thais-owned changes only</i>]
        G -->|allow| T
        G -->|allow| S
        S -.->|every tool call| G
        T -->|result| O
        S -->|summary or diff| O
        O -->|code mutation| V
        V -->|pass| O
        V -->|fail| R
        R -->|restored| O
    end

    subgraph MEM["Memory and history"]
        LOG[(Event Log<br/><i>events table in thais.db</i>)]
        DB[(SQLite FTS5<br/><i>derived events_fts index in thais.db</i>)]
        MD[MEMORY.md<br/><i>approved rules and decisions</i>]
        CK[checkpoint.md<br/><i>runtime-owned state, plan, tasks</i>]
        RC[Context Rebuild<br/><i>safe point, write-ahead</i>]
        LOG -->|index events| DB
        MD --> RC
        CK --> RC
        LOG --> RC
    end

    U --> TUI
    TUI -->|commands| API
    API -->|events| TUI
    API -->|dispatch| O
    O -->|events| API
    P -->|show plan| API
    API -->|plan decision| P
    O -->|proposed action| G
    G -->|ask| API
    G -->|deny| O
    G -->|allow memory| O
    O -->|approved memory write| MD
    O -->|append events| LOG
    O -->|state, plan, tasks| CK
    O -->|safe point, checkpoint| RC
    LOG -->|recent window| C
    DB -.->|search on demand| T
    MD -->|always| C
    CK -->|session start| C
    RC -->|compact context| C
```

Um único boundary de cliente. Um único loop de responsabilidade. O runtime é dono do estado,
do log e dos checkpoints; clientes falam JSON-RPC e recebem eventos.

## Como funciona

| Etapa | O que acontece |
|---|---|
| **Assemble** | O Context Manager monta o prompt com segurança, `AGENTS.md`, `MEMORY.md`, checkpoint e histórico recente. |
| **Decide** | O modelo propõe um plano, tool calls, um subagente ou a resposta final. |
| **Authorize** | O gate aplica modo e política a cada ação. `ask` vai para você; `deny` volta com motivo. |
| **Act** | Ferramentas e subagentes executam. Leituras podem ser paralelas; mutações são serializadas. |
| **Verify** | Verifier e LSP checam mudanças de código. Falha restaura o snapshot e reduz o budget de reparo. |
| **Persist** | O checkpoint é escrito, o uso é contabilizado e os eventos já estão no log append-only. |

## Começando

Requisitos: Rust 1.99+, `git` e as dependências de build da sua plataforma.

```sh
git clone git@github.com:marcosgabrielgomes110-collab/Thais.git
cd Thais
make
```

O provider fake não precisa de chave:

```sh
cargo run -p thais-cli -- tui --provider fake
```

Para usar um provider real:

```sh
export THAIS_COHERE_API_KEY="..."
cargo run -p thais-cli -- tui --provider cohere
```

Ou, com o Makefile:

```sh
make tui cohere
make tui cohere MODE=plan
```

### Teclas da TUI

`Enter` envia, `Esc` cancela o turno ativo ou sai, `PageUp`/`PageDown` e a roda
do mouse rolam, `Home`/`End` vão ao topo/fim, `Ctrl+S` abre o seletor de sessões
e `Ctrl+C` encerra. Digite `/` para comandos com autocomplete: `/help`, `/new`,
`/sessions`, `/model`, `/history`, `/status`, `/clear`, `/cancel`, `/approve`,
`/deny`, `/connect` e `/quit`. Use `Tab` para completar e `Up`/`Down` para escolher. `/connect <provider> [model]` pede a API key sem exibi-la e reinicia a TUI com os novos modelos. A TUI
usa poucas cores, sem fundos, e libera o lease ao sair.

### Headless

```sh
thais run --task "Find where Runtime is defined and cite the file"
thais run --task "..." --json
```

`stdout` é só protocolo; logs vão para `stderr`.

## Provedores e modelos

A configuração é sua, não do repositório:

```sh
cp docs/providers.example.toml ~/.config/thais/providers.toml
cp docs/models.example.toml ~/.config/thais/models.toml
thais auth login openai --account default
thais models
```

Presets disponíveis:

`openai` · `anthropic` · `cohere` · `openrouter` · `deepseek` · `qwen` · `zai` · `ollama` · `vllm` · `litellm`

Credenciais vêm do keyring, de variável de ambiente ou do fallback `0600`.
`providers.toml` nunca contém segredo.

Model roles: `default`, `plan`, `build`, `explore`, `general`, `compact`.
Cada role pode ter fallback ordenado; trocas e fallbacks são registrados.
O custo por turno tem aviso em 80% e parada em 100% (padrão: US$ 2).

## Sessões, memória e continuidade

```sh
thais sessions --all
thais history "decisão"
thais lease status --workspace .
thais lease release --workspace .
thais daemon --addr 127.0.0.1:8787
thais events --follow
```

- Checkpoints ficam em `~/.local/share/thais/sessions/<id>/checkpoint.md`.
- O Event Log fica em `~/.local/share/thais/thais.db` e é a fonte de verdade.
- `MEMORY.md` fica no root do projeto e só é escrito após aprovação com diff.
- Context Rebuild preserva segurança, `AGENTS.md`, badge e memória; os turnos
  recentes ficam verbatim e os antigos são resumidos.
- Aprovações pendentes sobrevivem ao restart; o turno em voo não é reexecutado.

## Ferramentas e integrações

- **LSP:** `lsp_diagnostics`, `lsp_definition`, `lsp_references`.
  Configure `~/.config/thais/lsp.toml`; veja `docs/lsp.example.toml`.
- **MCP:** ferramentas namespaced como `mcp__server__tool`.
  Configure `~/.config/thais/mcp.toml`; veja `docs/mcp.example.toml`.
- **Subagentes:** `explore` read-only e `general` com gate, snapshot,
  verificação e rollback. Contexto isolado e profundidade 1.
- **Filesystem:** `delete`, `move` e `mkdir` com path jail, preview e aprovação.
- **Memória:** `memory_write`, `memory_read` e `memory_search`.
- **Git:** `git_status`, `git_diff`, `git_log`, `git_show`, `git_branch`, `git_checkout`, `git_stash` e `git_commit`.
- **Segredos:** `secret_scan` e bloqueio automático no `git_commit`.
- **Jobs e verificação:** `bash_start`, `bash_logs`, `bash_stop`, `test` e `build`; logs completos em `target/`, resumo compacto para o modelo.
  Commit é gated e nunca faz amend ou reescreve histórico.

## Segurança

- O gate decide `allow`, `ask` ou `deny` para toda ação.
- Plan mode é read-only; aprovação do plano é a única entrada em build mode.
- `bash` continua `ask`; sandbox ativo não remove a aprovação nesta fase. O path jail **não** é um sandbox de SO.
- Mutações têm snapshot e rollback; o Verifier cobre apenas mudanças de código.
- Segredos são redigidos antes do log, do índice e do prompt.
- Config de projeto é não confiável e não pode definir providers, auth,
  permissões, comandos de verifier ou sandbox.
- Nenhuma telemetria.

## Status do projeto

| Marco | Estado | Release |
|---|---|---|
| M0 — Foundation | Done | `v0.1.0-m0` |
| M1 — Read-only agent | Done | `v0.2.0-m1` |
| M2 — Safe build | Done | `v0.3.0-m2` |
| M3 — Sessions and models | Done | `v0.4.0-m3` |
| M4 — Memory and continuity | Done | `v0.5.0-m4` |
| M5 — Tool ecosystem | Done | `v0.6.0-m5` |
| Alpha | Feedback gate | em uso real |
| M6 — Hardening | Done no Linux | `v0.7.0-m6` |
| M7 — Productization | Done | `v0.8.0-m7` |

O Alpha continua sendo o gate de feedback em repositório real. O **M6 está
fechado no Linux**; o escopo do v1.0 é **somente Linux**.

### M6 hardening — Linux

Já implementado:

- Sandbox Linux:
  - `bwrap` com namespaces quando disponível;
  - fallback Landlock + seccomp via `thais-sandbox`, com filesystem restrito, rede bloqueada e syscalls perigosas negadas;
  - `THAIS_SANDBOX=auto|off|required`;
  - fallback seguro mantendo `bash` em `ask`;
  - status do backend no `thais doctor`.
- `thais-sandbox` é instalado junto com o binário e aplica Landlock antes do `exec`.
- Redaction de tool output e diffs antes do modelo e do Event Log.
- Expansão dos padrões de segredo.
- Testes de prompt injection, redaction e recuperação de lease expirado.
- TUI limita o transcript renderizado para evitar crescimento sem limite.

### M7 productization

- Daemon WebSocket local em `127.0.0.1` com token obrigatório.
- Múltiplos clientes por sessão, com eventos compartilhados e aprovação first-wins.
- `thais events --follow` e `thais events --session <id>`.
- Extensão VS Code em `extensions/vscode`: conectar, criar sessão, enviar task,
  trocar sessão, escolher modelo e aprovar ações.
- `dist-workspace.toml`, `Dockerfile`, `docs/install.md` e `CHANGELOG.md`.
- Empacotamento Linux, Docker e extensão VS Code.

O gate v1.0 Linux ainda precisa de fuzzing/loom, assinatura/SBOM e do pass de
performance. macOS e Windows ficam fora do escopo do v1.0.

## Documentação

| Documento | Conteúdo |
|---|---|
| [docs/protocol.md](docs/protocol.md) | Agent API, métodos, eventos e fluxo de build |
| [docs/security.md](docs/security.md) | modelo de segurança, sandbox e riscos residuais |
| [ROADMAP.md](ROADMAP.md) | marcos, critérios de aceite e decisões |
| [docs/adr/](docs/adr/) | Architecture Decision Records |
| [docs/providers.example.toml](docs/providers.example.toml) | providers e presets |
| [docs/models.example.toml](docs/models.example.toml) | catálogo e custos |
| [docs/lsp.example.toml](docs/lsp.example.toml) | servidores LSP |
| [docs/mcp.example.toml](docs/mcp.example.toml) | servidores MCP |
| [docs/verifier.example.toml](docs/verifier.example.toml) | comandos de verificação |
| [PERSONALITY.md](PERSONALITY.md) | tom e estilo do agente |
| [docs/install.md](docs/install.md) | instalação, Docker e artefatos |
| [CHANGELOG.md](CHANGELOG.md) | histórico de releases |
| [docs/release.md](docs/release.md) | build, SBOM, assinatura e verificação |

## Desenvolvimento

```sh
make check      # fmt, clippy, testes e boundary arquitetural
make build      # release build
cargo deny check
make sbom       # CycloneDX SBOM
make fuzz-check # type-check fuzz targets
scripts/release.sh v1.0.0 --allow-unsigned  # local release check
scripts/verify-release.sh
scripts/dist-local.sh v0.1.0  # cargo-dist archives, checksums, sh installers
scripts/perf.sh  # first task and event-log latency
```

A CI roda em Linux, além de audit de dependências.
`thais-tui` depende apenas de `thais-sdk` e `thais-protocol`; o boundary é
testado, não apenas documentado.

---

<p align="center">
  <sub>Feito em Rust. Roda local. Mantém você no controle.</sub>
</p>
