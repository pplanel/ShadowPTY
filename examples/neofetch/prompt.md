# Example: Nix-Shell with System Information (`fastfetch`)

This example demonstrates using **ShadowPTY** (`termcp`) to spawn an interactive shell in a pseudo-terminal, record the session in asciicast v3 format, inspect the rendered screen with semantic ANSI tags, send keystrokes, and cleanly terminate the session.

---

## 1. Session Objective

1. Spawn an isolated Nix environment (`nix-shell -p fastfetch`) inside a dedicated PTY with window dimensions of 35 rows by 100 columns.
2. Record the interactive session into `recording.cast` (asciicast v3 format).
3. Wait for the shell prompt (`[nix-shell:~]$ `).
4. Send the command `fastfetch<ENTER>`.
5. Capture and verify the rendered terminal screen (including system specs, memory usage, colors, and layout).
6. Send `exit<ENTER>` to terminate the shell and finalize the recording.

---

## 2. MCP Tool Execution Sequence

### Step 1: Start PTY and Recording (`tui_start`)

```json
{
  "command": "nix-shell",
  "args": ["-p", "fastfetch"],
  "rows": 35,
  "cols": 100,
  "record_path": "/Users/pplanel/src/termcp/examples/neofetch/recording.cast"
}
```

**Result:**
```text
Started command 'nix-shell' in PTY (pid: 48644, rows: 35, cols: 100, recording to '/Users/pplanel/src/termcp/examples/neofetch/recording.cast')
```

---

### Step 2: Verify Initial Shell Prompt (`tui_read`)

```json
{}
```

**Screen State:**
```text
<fg:green><bold>[nix-shell:~]$</bold></fg>
```

---

### Step 3: Send Command (`tui_input`)

```json
{
  "keys": "fastfetch<ENTER>"
}
```

**Result:**
```text
Sent 10 bytes to PTY for input "fastfetch<ENTER>"
```

---

### Step 4: Inspect Rendered Output (`tui_read`)

```json
{}
```

**Captured Terminal Output:**
```text
<fg:green><bold>[nix-shell:~]$</bold></fg> fastfetch
<fg:green><bold>                     ..'</bold></fg>          <fg:green><bold>pplanel</bold></fg>@<fg:green><bold>caladan</bold></fg>
<fg:green><bold>                 ,xNMM.</bold></fg>           ---------------
<fg:green><bold>               .OMMMMo</bold></fg>            <fg:yellow><bold>OS</bold></fg>: macOS Golden Gate 27.0 (26A428) arm64
<fg:green><bold>               lMM"</bold></fg>               <fg:yellow><bold>Host</bold></fg>: MacBook Pro (16-inch, 2021)
<fg:green><bold>     .;loddo:.  .olloddol;.</bold></fg>       <fg:yellow><bold>Kernel</bold></fg>: Darwin 27.0.0
<fg:green><bold>   cKMMMMMMMMMMNWMMMMMMMMMM0:</bold></fg>     <fg:yellow><bold>Uptime</bold></fg>: 5 days, 15 hours, 10 mins
<fg:green><bold> </bold></fg><fg:yellow><bold>.KMMMMMMMMMMMMMMMMMMMMMMMWd.</bold></fg>     <fg:yellow><bold>Packages</bold></fg>: 137 (brew), 10 (brew-cask), 357 (nix-system), 54 (nix-default)
<fg:yellow><bold> XMMMMMMMMMMMMMMMMMMMMMMMX.</bold></fg>       <fg:yellow><bold>Shell</bold></fg>: bash 5.3.15
<fg:bright-red><bold>;MMMMMMMMMMMMMMMMMMMMMMMM:</bold></fg>        <fg:yellow><bold>Display (Color LCD)</bold></fg>: 3456x2234 @ 2x in 16", 120 Hz [Built-in]
<fg:bright-red><bold>:MMMMMMMMMMMMMMMMMMMMMMMM:</bold></fg>        <fg:yellow><bold>WM</bold></fg>: Quartz Compositor 1.600.0 (with AeroSpace 0.21.3-Beta)
<fg:red><bold>.MMMMMMMMMMMMMMMMMMMMMMMMX.</bold></fg>       <fg:yellow><bold>WM Theme</bold></fg>: Multicolor (Dark)
<fg:red><bold> kMMMMMMMMMMMMMMMMMMMMMMMMWd.</bold></fg>     <fg:yellow><bold>Theme</bold></fg>: Liquid Glass
<fg:red><bold> </bold></fg><fg:magenta><bold>'XMMMMMMMMMMMMMMMMMMMMMMMMMMk</bold></fg>    <fg:yellow><bold>Font</bold></fg>: .AppleSystemUIFont [System], Helvetica [User]
<fg:magenta><bold>  'XMMMMMMMMMMMMMMMMMMMMMMMMK.</bold></fg>    <fg:yellow><bold>Cursor</bold></fg>: Fill - Black, Outline - White (32px)
<fg:magenta><bold>    </bold></fg><fg:blue><bold>kMMMMMMMMMMMMMMMMMMMMMMd</bold></fg>      <fg:yellow><bold>Terminal</bold></fg>: termcp
<fg:blue><bold>     ;KMMMMMMMWXXWMMMMMMMk.</bold></fg>       <fg:yellow><bold>CPU</bold></fg>: Apple M1 Pro (8+2) @ 3.23 GHz
<fg:blue><bold>       "cooc*"    "*coo'"</bold></fg>         <fg:yellow><bold>GPU</bold></fg>: Apple M1 Pro (16) @ 1.30 GHz [Integrated]
                                  <fg:yellow><bold>Memory</bold></fg>: 12.20 GiB / 16.00 GiB (<fg:bright-yellow>76%</fg>)
                                  <fg:yellow><bold>Swap</bold></fg>: 901.69 MiB / 2.00 GiB (<fg:green>44%</fg>)
                                  <fg:yellow><bold>Disk (/)</bold></fg>: 388.65 GiB / 460.43 GiB (<fg:bright-red>84%</fg>) - apfs [Read-only]
                                  <fg:yellow><bold>Disk (/Volumes/Ollama)</bold></fg>: 596.78 MiB / 649.00 MiB (<fg:bright-red>92%</fg>) - hfs [External, Read-only]
                                  <fg:yellow><bold>Local IP (en0)</bold></fg>: 10.0.1.37/22
                                  <fg:yellow><bold>Battery (bq40z651)</bold></fg>: <fg:green>92%</fg> (8 hours, 7 mins remaining) [Discharging]
                                  <fg:yellow><bold>Locale</bold></fg>: en_US.UTF-8
                                  
                                  <bg:black>   </bg><bg:red>   </bg><bg:green>   </bg><bg:yellow>   </bg><bg:blue>   </bg><bg:magenta>   </bg><bg:cyan>   </bg><bg:white>   </bg>
                                  <bg:bright-black>   </bg><bg:bright-red>   </bg><bg:bright-green>   </bg><bg:bright-yellow>   </bg><bg:bright-blue>   </bg><bg:bright-magenta>   </bg><bg:bright-cyan>   </bg><bg:bright-white>   </bg>

<fg:green><bold>[nix-shell:~]$</bold></fg>
```

---

### Step 5: Clean Exit (`tui_input`)

```json
{
  "keys": "exit<ENTER>"
}
```

**Result:**
```text
Sent 5 bytes to PTY for input "exit<ENTER>"
```

---

## 3. Replaying the Recording

The generated `recording.cast` is a standards-compliant asciicast v3 file. You can replay it in any terminal using:

```bash
# Using asciinema CLI:
asciinema play recording.cast
```
