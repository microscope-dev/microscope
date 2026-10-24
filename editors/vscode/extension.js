// @ts-check
// Starts `microscope dap` for microscope debug sessions, opens a launch.json when
// there is no configuration to debug, offers the workspace's programs to
// launch, picks processes to attach to, and shows values in the editor:
// whole expressions on hover, and each variable's value on the lines where
// the stopped frame's variable is the one in scope.

'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const vscode = require('vscode');

const LANGUAGES = [
    'c', 'cpp', 'rust', 'go', 'zig', 'python', 'javascript', 'typescript', 'javascriptreact', 'typescriptreact',
];

/** The microscope sessions running now. */
const sessions = new Set();

/** @param {vscode.ExtensionContext} context */
function activate(context) {
    const languages = LANGUAGES.map((language) => ({ language }));
    context.subscriptions.push(
        vscode.debug.registerDebugAdapterDescriptorFactory('microscope', { createDebugAdapterDescriptor }),
        vscode.debug.registerDebugConfigurationProvider('microscope', { resolveDebugConfiguration }),
        vscode.debug.registerDebugConfigurationProvider(
            'microscope',
            { provideDebugConfigurations },
            vscode.DebugConfigurationProviderTriggerKind.Dynamic,
        ),
        vscode.commands.registerCommand('microscope.pickProcess', pickProcess),
        vscode.commands.registerCommand('microscope.toggleHexadecimal', toggleHexadecimal),
        vscode.languages.registerEvaluatableExpressionProvider(languages, { provideEvaluatableExpression }),
        vscode.languages.registerInlineValuesProvider(languages, { provideInlineValues }),
        vscode.debug.onDidStartDebugSession((session) => {
            if (session.type === 'microscope') {
                sessions.add(session);
                if (hexadecimal()) {
                    sendValueFormat(session);
                }
            }
        }),
        vscode.debug.onDidTerminateDebugSession((session) => sessions.delete(session)),
        vscode.workspace.onDidChangeConfiguration((event) => {
            if (event.affectsConfiguration('microscope.hexadecimal')) {
                updateHexadecimal();
            }
        }),
    );
    updateHexadecimal();
}

/** @param {vscode.DebugSession} session */
function createDebugAdapterDescriptor(session) {
    // A process the program started, which the adapter that debugs the
    // program hosts: its session connects back to that adapter.
    const member = session.configuration.__microscopeMember;
    if (member !== undefined) {
        return new vscode.DebugAdapterNamedPipeServer(member.socket);
    }
    const folder = session.workspaceFolder;
    const settings = vscode.workspace.getConfiguration('microscope', folder);
    const setting = settings.get('path', 'microscope');
    const command = findExecutable(setting, folder?.uri.fsPath);
    if (command === undefined) {
        throw new Error(`Cannot find microscope at "${setting}". Install microscope on PATH or set microscope.path.`);
    }
    const args = ['dap'];
    const log = settings.get('logFile', '');
    if (log) {
        args.push('--log', expandPath(log, folder?.uri.fsPath));
    }
    return new vscode.DebugAdapterExecutable(command, args);
}

/**
 * Expands ${workspaceFolder} and ${userHome} in a setting's path, and
 * resolves a relative path against the workspace folder.
 *
 * @param {string} setting
 * @param {string | undefined} workspaceFolder
 */
function expandPath(setting, workspaceFolder) {
    const expanded = setting
        .replaceAll('${workspaceFolder}', workspaceFolder ?? '')
        .replaceAll('${userHome}', os.homedir());
    return path.resolve(workspaceFolder ?? process.cwd(), expanded);
}

/**
 * Resolves the microscope.path setting to an executable file: a name searched
 * for on PATH, or a path that may start with ${workspaceFolder} or ${userHome}.
 * Relative paths start at the workspace folder.
 *
 * @param {string} setting
 * @param {string | undefined} workspaceFolder
 * @returns {string | undefined}
 */
function findExecutable(setting, workspaceFolder) {
    if (setting.includes('${workspaceFolder}') && workspaceFolder === undefined) {
        return undefined;
    }
    const candidates = setting.includes('/')
        ? [expandPath(setting, workspaceFolder)]
        : (process.env.PATH ?? '').split(path.delimiter).filter(Boolean).map((directory) => path.join(directory, setting));
    return candidates.find(isExecutableFile);
}

/** @param {string} file */
function isExecutableFile(file) {
    try {
        fs.accessSync(file, fs.constants.X_OK);
        return fs.statSync(file).isFile();
    } catch {
        return false;
    }
}

/**
 * Pressing F5 without a launch.json passes an empty configuration; returning
 * null asks VS Code to create launch.json from the initial configurations.
 *
 * @param {vscode.WorkspaceFolder | undefined} folder
 * @param {vscode.DebugConfiguration} configuration
 */
function resolveDebugConfiguration(folder, configuration) {
    if (configuration.request !== undefined) {
        return configuration;
    }
    if (folder === undefined) {
        vscode.window.showErrorMessage('Open a folder to create a launch.json for microscope.');
        return undefined;
    }
    return null;
}

/** Directories no program to debug is built into. */
const SKIPPED_DIRECTORIES = new Set(['node_modules', 'deps', 'build-script-build', 'incremental', '.fingerprint']);
/** How deep, and through how many entries, the workspace is searched. */
const PROGRAM_SEARCH = { depth: 4, entries: 5000, programs: 50 };

/**
 * Offers a launch configuration for each program built in the folder, in
 * the Run and Debug view's list of dynamic configurations.
 *
 * @param {vscode.WorkspaceFolder | undefined} folder
 */
function provideDebugConfigurations(folder) {
    if (folder === undefined) {
        return [];
    }
    return programs(folder.uri.fsPath).map((program) => {
        const relative = path.relative(folder.uri.fsPath, program);
        return {
            type: 'microscope',
            request: 'launch',
            name: `microscope: ${relative}`,
            program: `\${workspaceFolder}/${relative}`,
            cwd: '${workspaceFolder}',
        };
    });
}

/**
 * The ELF executables under a directory, nearest first: executable files
 * that are programs rather than shared libraries, outside hidden and
 * dependency directories, within a bounded search.
 *
 * @param {string} root
 * @returns {string[]}
 */
function programs(root) {
    const found = [];
    let entries = 0;
    let level = [root];
    for (let depth = 0; depth <= PROGRAM_SEARCH.depth && level.length > 0; depth++) {
        const next = [];
        for (const directory of level) {
            let children;
            try {
                children = fs.readdirSync(directory, { withFileTypes: true })
                    .sort((first, second) => first.name.localeCompare(second.name));
            } catch {
                continue;
            }
            for (const child of children) {
                if (++entries > PROGRAM_SEARCH.entries || found.length >= PROGRAM_SEARCH.programs) {
                    return found;
                }
                const file = path.join(directory, child.name);
                if (child.name.startsWith('.')) {
                    continue;
                } else if (child.isDirectory()) {
                    if (!SKIPPED_DIRECTORIES.has(child.name)) {
                        next.push(file);
                    }
                } else if (child.isFile() && isExecutableFile(file) && isProgram(file)) {
                    found.push(file);
                }
            }
        }
        level = next;
    }
    return found;
}

/**
 * Whether a file is an x86-64 ELF program: an executable, or a position
 * independent one, which unlike a shared library names an interpreter.
 *
 * @param {string} file
 */
function isProgram(file) {
    let descriptor;
    try {
        descriptor = fs.openSync(file, 'r');
        const header = Buffer.alloc(64);
        if (fs.readSync(descriptor, header, 0, 64, 0) < 64
            || header.readUInt32BE(0) !== 0x7f454c46 // \x7fELF
            || header[4] !== 2 // 64-bit
            || header[5] !== 1 // little-endian
            || header.readUInt16LE(18) !== 62) { // x86-64
            return false;
        }
        const type = header.readUInt16LE(16);
        if (type === 2) { // ET_EXEC
            return true;
        }
        if (type !== 3) { // ET_DYN
            return false;
        }
        const offset = Number(header.readBigUInt64LE(32));
        const size = header.readUInt16LE(54);
        const count = header.readUInt16LE(56);
        if (size < 4 || count > 256) {
            return false;
        }
        const headers = Buffer.alloc(size * count);
        if (fs.readSync(descriptor, headers, 0, headers.length, offset) < headers.length) {
            return false;
        }
        for (let index = 0; index < count; index++) {
            if (headers.readUInt32LE(index * size) === 3) { // PT_INTERP
                return true;
            }
        }
        return false;
    } catch {
        return false;
    } finally {
        if (descriptor !== undefined) {
            fs.closeSync(descriptor);
        }
    }
}

/**
 * Lists the user's processes, newest first, for ${command:pickProcess}: only
 * those running the configuration's program, when it names one. Returns the
 * chosen process id, or undefined when the pick is dismissed, which cancels
 * the debug session.
 *
 * @param {vscode.DebugConfiguration | undefined} configuration
 */
async function pickProcess(configuration) {
    // VS Code has substituted the configuration's other variables already.
    const program = typeof configuration?.program === 'string' ? realPath(configuration.program) : undefined;
    const items = processes()
        .filter((entry) => program === undefined || entry.executable === program)
        .map((entry) => ({
            label: entry.name,
            description: String(entry.pid),
            detail: entry.commandLine,
            pid: entry.pid,
        }));
    const picked = await vscode.window.showQuickPick(items, {
        title: program === undefined ? 'Attach to a process' : `Attach to a process running ${program}`,
        placeHolder: 'Filter by name, process ID, or command line',
        matchOnDescription: true,
        matchOnDetail: true,
    });
    return picked === undefined ? undefined : String(picked.pid);
}

/** @param {string} file */
function realPath(file) {
    try {
        return fs.realpathSync(file);
    } catch {
        return file;
    }
}

/**
 * The processes the current user owns, except this extension host, which
 * stopping would hang, newest first.
 *
 * @returns {{ pid: number, name: string, commandLine: string, executable?: string, started: number }[]}
 */
function processes() {
    const uid = process.getuid?.();
    const found = [];
    for (const entry of fs.readdirSync('/proc')) {
        const pid = Number(entry);
        if (!Number.isInteger(pid) || pid === process.pid) {
            continue;
        }
        try {
            const directory = `/proc/${pid}`;
            if (fs.statSync(directory).uid !== uid) {
                continue;
            }
            // The fields follow the parenthesized name, which may itself contain
            // spaces and parentheses. fields[0] is the state, field 3 of stat,
            // so field 22, the start time, is fields[19].
            const stat = fs.readFileSync(`${directory}/stat`, 'utf8');
            const fields = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
            // A zombie has exited and cannot be traced.
            if (fields[0] === 'Z') {
                continue;
            }
            const name = fs.readFileSync(`${directory}/comm`, 'utf8').trimEnd();
            const commandLine = fs.readFileSync(`${directory}/cmdline`, 'utf8').split('\0').filter(Boolean).join(' ');
            let executable;
            try {
                executable = fs.readlinkSync(`${directory}/exe`);
            } catch {
                // The process has no executable to read, or it just exited.
            }
            found.push({ pid, name, commandLine: commandLine || `[${name}]`, executable, started: Number(fields[19]) });
        } catch {
            // The process exited while it was being read.
        }
    }
    return found.sort((first, second) => second.started - first.started || second.pid - first.pid);
}

const hexadecimal = () => vscode.workspace.getConfiguration('microscope').get('hexadecimal', false);

/** Flips the microscope.hexadecimal setting, which every microscope session follows. */
async function toggleHexadecimal() {
    await vscode.workspace.getConfiguration('microscope')
        .update('hexadecimal', !hexadecimal(), vscode.ConfigurationTarget.Global);
}

/** Shows the setting's state to menus and tells running sessions. */
function updateHexadecimal() {
    vscode.commands.executeCommand('setContext', 'microscope.hexadecimal', hexadecimal());
    for (const session of sessions) {
        sendValueFormat(session);
    }
}

/** @param {vscode.DebugSession} session */
function sendValueFormat(session) {
    session.customRequest('microscope/setValueFormat', { hex: hexadecimal() }).then(undefined, () => {
        // The session ended, or has not started a program yet; a new one is
        // told when it starts.
    });
}


/**
 * The expression a hover at a position names: the chain of names, members,
 * pointers, and indices that ends with the hovered name, such as
 * `p->items[i].next` when hovering `next`, or `ns::value`.
 *
 * @param {vscode.TextDocument} document
 * @param {vscode.Position} position
 */
function provideEvaluatableExpression(document, position) {
    if (vscode.debug.activeDebugSession?.type !== 'microscope') {
        return undefined;
    }
    const found = expressionAt(document.lineAt(position.line).text, position.character);
    if (found === undefined) {
        return undefined;
    }
    const range = new vscode.Range(position.line, found.start, position.line, found.end);
    return new vscode.EvaluatableExpression(range, found.expression);
}

const NAME = /[A-Za-z0-9_]/;

/**
 * The expression ending with the name at a column of a line, and where it
 * starts and ends. Undefined where the column is on no name.
 *
 * @param {string} text
 * @param {number} column
 * @returns {{ expression: string, start: number, end: number } | undefined}
 */
function expressionAt(text, column) {
    let start = column;
    let end = column;
    while (start > 0 && NAME.test(text[start - 1])) {
        start--;
    }
    while (end < text.length && NAME.test(text[end])) {
        end++;
    }
    if (start === end || /[0-9]/.test(text[start])) {
        return undefined;
    }
    // Walk back over what joins the name to the operands before it.
    for (;;) {
        let at = start;
        if (text.startsWith('->', at - 2)) {
            at -= 2;
        } else if (text.startsWith('::', at - 2)) {
            at -= 2;
            if (at === 0 || !NAME.test(text[at - 1])) {
                start = at;
                break;
            }
        } else if (text[at - 1] === '.') {
            at -= 1;
        } else {
            break;
        }
        // Indices of the operand, then its name.
        while (text[at - 1] === ']') {
            const open = opening(text, at - 1);
            if (open === undefined) {
                return { expression: text.slice(start, end), start, end };
            }
            at = open;
        }
        if (text[at - 1] === ')') {
            const open = opening(text, at - 1);
            if (open === undefined) {
                break;
            }
            // A call's result, which the debugger refuses rather than
            // guessing, or a length or size, is one operand with its name.
            at = open;
            while (at > 0 && NAME.test(text[at - 1])) {
                at--;
            }
        } else {
            const name = at;
            while (at > 0 && NAME.test(text[at - 1])) {
                at--;
            }
            if (at === name) {
                break;
            }
        }
        start = at;
    }
    if (text[start - 1] === '$') {
        start--;
    }
    return { expression: text.slice(start, end), start, end };
}

/**
 * The column of the bracket that opens the group closing at a column.
 *
 * @param {string} text
 * @param {number} close
 */
function opening(text, close) {
    const pairs = { ']': '[', ')': '(' };
    const stack = [];
    for (let index = close; index >= 0; index--) {
        const character = text[index];
        if (character in pairs) {
            stack.push(pairs[character]);
        } else if (character === '[' || character === '(') {
            if (stack.pop() !== character) {
                return undefined;
            }
            if (stack.length === 0) {
                return index;
            }
        }
    }
    return undefined;
}

/**
 * Shows the stopped frame's variables on the lines that use them: for each
 * variable the debugger declares in this file, the lines from its
 * declaration to the stop, except lines in blocks that close before the
 * stop, where the name may be another variable.
 *
 * @param {vscode.TextDocument} document
 * @param {vscode.Range} viewPort
 * @param {vscode.InlineValueContext} context
 */
async function provideInlineValues(document, viewPort, context) {
    const session = vscode.debug.activeDebugSession;
    if (session?.type !== 'microscope') {
        return undefined;
    }
    try {
        return await inlineValues(session, document, viewPort, context);
    } catch {
        // The program ran on, and the stop's references are gone.
        return undefined;
    }
}

/**
 * @param {vscode.DebugSession} session
 * @param {vscode.TextDocument} document
 * @param {vscode.Range} viewPort
 * @param {vscode.InlineValueContext} context
 */
async function inlineValues(session, document, viewPort, context) {
    const stop = context.stoppedLocation.end.line;
    const lines = document.getText(new vscode.Range(0, 0, stop + 1, 0)).split('\n');
    const values = [];
    const { scopes } = await session.customRequest('scopes', { frameId: context.frameId });
    for (const scope of scopes) {
        if (scope.expensive || !scope.variablesReference) {
            continue;
        }
        const { variables } = await session.customRequest('variables', { variablesReference: scope.variablesReference });
        for (const variable of variables) {
            // A variable without a name to evaluate is one another hides.
            if (!variable.declarationLocationReference || !variable.evaluateName) {
                continue;
            }
            const declared = await session.customRequest('locations', { locationReference: variable.declarationLocationReference });
            if (!declared.source?.path || realPath(declared.source.path) !== realPath(document.uri.fsPath)) {
                continue;
            }
            for (const use of uses(lines, variable.name, declared.line - 1, stop)) {
                if (use.line >= viewPort.start.line && use.line <= viewPort.end.line) {
                    const range = new vscode.Range(use.line, use.start, use.line, use.start + variable.name.length);
                    values.push(new vscode.InlineValueText(range, `${variable.name} = ${variable.value}`));
                }
            }
        }
    }
    return values;
}

/**
 * The first use of a name on each line from a declaration's line to the
 * stop's line, outside comments, strings, and member selections, and
 * outside blocks that close before the stop. None when the declaration's
 * own block closes before the stop, which means the name is not the
 * declared variable there.
 *
 * @param {string[]} lines
 * @param {string} name
 * @param {number} first
 * @param {number} last
 * @returns {{ line: number, start: number }[]}
 */
function uses(lines, name, first, last) {
    const found = [];
    // Each line's block depth where it starts, and its first use.
    const depths = [];
    let depth = 0;
    let comment = false;
    for (let line = first; line <= last && line < lines.length; line++) {
        depths.push({ line, depth, start: undefined, lowest: depth });
        const entry = depths[depths.length - 1];
        const text = lines[line];
        for (let column = 0; column < text.length; column++) {
            const character = text[column];
            if (comment) {
                if (text.startsWith('*/', column)) {
                    comment = false;
                    column++;
                }
            } else if (text.startsWith('/*', column)) {
                comment = true;
                column++;
            } else if (text.startsWith('//', column) || text.startsWith('\\\\', column)) {
                break;
            } else if (character === '"' || character === '`') {
                column = closing(text, column, character);
            } else if (character === '\'' && /^'(\\.[^']*|[^\\'])'/.test(text.slice(column))) {
                column = closing(text, column, '\'');
            } else if (character === '{') {
                depth++;
            } else if (character === '}') {
                depth--;
                entry.lowest = Math.min(entry.lowest, depth);
            } else if (NAME.test(character) && (column === 0 || !NAME.test(text[column - 1]))) {
                let end = column;
                while (end < text.length && NAME.test(text[end])) {
                    end++;
                }
                const selected = /(\.|->|::)\s*$/.test(text.slice(0, column));
                if (entry.start === undefined && !selected && text.slice(column, end) === name) {
                    entry.start = column;
                }
                column = end - 1;
            }
        }
    }
    // A line is in a block that closes before the stop when the depth after
    // it falls below the depth it starts at.
    let lowest = depth;
    for (let index = depths.length - 1; index >= 0; index--) {
        const entry = depths[index];
        lowest = Math.min(lowest, entry.lowest);
        if (index === 0 && lowest < 0) {
            return [];
        }
        if (entry.start !== undefined && entry.depth <= lowest) {
            found.push({ line: entry.line, start: entry.start });
        }
        lowest = Math.min(lowest, entry.depth);
    }
    return found.reverse();
}

/**
 * The column of the quote that closes a string or character literal.
 *
 * @param {string} text
 * @param {number} open
 * @param {string} quote
 */
function closing(text, open, quote) {
    for (let column = open + 1; column < text.length; column++) {
        if (text[column] === '\\' && quote !== '`') {
            column++;
        } else if (text[column] === quote) {
            return column;
        }
    }
    return text.length;
}

module.exports = {
    activate, findExecutable, processes, programs, expressionAt, uses, provideInlineValues,
    provideDebugConfigurations,
};
