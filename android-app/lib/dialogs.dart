import 'package:flutter/material.dart';

import 'operations.dart';

Future<LaunchOptions?> showLaunchDialog(
  BuildContext context, {
  bool fork = false,
}) async {
  final directory = TextEditingController();
  final account = TextEditingController();
  final environment = TextEditingController();
  final environmentFiles = TextEditingController();
  final arguments = TextEditingController();
  final timeout = TextEditingController(text: '30');
  String? backend;
  var noWait = false;
  String? validation;
  final result = await showDialog<LaunchOptions>(
    context: context,
    builder: (context) => StatefulBuilder(
      builder: (context, setDialogState) => AlertDialog(
        title: Text(fork ? 'Fork agent' : 'Start agent'),
        content: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              TextField(
                controller: directory,
                decoration: const InputDecoration(
                  labelText: 'Working directory (optional)',
                  hintText: '~/code/project',
                ),
              ),
              if (!fork) ...[
                const SizedBox(height: 16),
                TextField(
                  controller: account,
                  decoration: const InputDecoration(
                    labelText: 'Account (optional)',
                  ),
                ),
                const SizedBox(height: 16),
                DropdownButtonFormField<String?>(
                  initialValue: backend,
                  decoration: const InputDecoration(labelText: 'Backend'),
                  items: const [
                    DropdownMenuItem(
                      value: null,
                      child: Text('Account default'),
                    ),
                    DropdownMenuItem(value: 'claude', child: Text('Claude')),
                    DropdownMenuItem(value: 'codex', child: Text('Codex')),
                    DropdownMenuItem(
                      value: 'opencode',
                      child: Text('OpenCode'),
                    ),
                    DropdownMenuItem(value: 'grok', child: Text('Grok')),
                  ],
                  onChanged: (value) => setDialogState(() => backend = value),
                ),
              ],
              const SizedBox(height: 8),
              ExpansionTile(
                tilePadding: EdgeInsets.zero,
                title: const Text('Advanced'),
                children: [
                  TextField(
                    controller: environment,
                    minLines: 2,
                    maxLines: 5,
                    autocorrect: false,
                    decoration: const InputDecoration(
                      labelText: 'Environment (one KEY=VALUE per line)',
                    ),
                  ),
                  const SizedBox(height: 16),
                  TextField(
                    controller: environmentFiles,
                    minLines: 1,
                    maxLines: 4,
                    autocorrect: false,
                    decoration: const InputDecoration(
                      labelText: 'Environment files (one path per line)',
                    ),
                  ),
                  const SizedBox(height: 16),
                  TextField(
                    controller: arguments,
                    minLines: 1,
                    maxLines: 4,
                    autocorrect: false,
                    decoration: const InputDecoration(
                      labelText: 'Agent arguments (one argument per line)',
                    ),
                  ),
                  SwitchListTile(
                    contentPadding: EdgeInsets.zero,
                    title: const Text('Return before agent is ready'),
                    subtitle: const Text(
                      'Use only when you want fire-and-forget behavior.',
                    ),
                    value: noWait,
                    onChanged: (value) => setDialogState(() => noWait = value),
                  ),
                  if (!noWait)
                    TextField(
                      controller: timeout,
                      keyboardType: TextInputType.number,
                      decoration: const InputDecoration(
                        labelText: 'Ready timeout (seconds)',
                      ),
                    ),
                ],
              ),
              if (validation != null)
                Padding(
                  padding: const EdgeInsets.only(top: 12),
                  child: Text(
                    validation!,
                    style: TextStyle(
                      color: Theme.of(context).colorScheme.error,
                    ),
                  ),
                ),
            ],
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () {
              final readyTimeout = int.tryParse(timeout.text.trim());
              final env = nonEmptyLines(environment.text);
              if (!noWait && (readyTimeout == null || readyTimeout < 1)) {
                setDialogState(
                  () => validation = 'Ready timeout must be at least 1 second.',
                );
                return;
              }
              if (env.any((value) => !value.contains('='))) {
                setDialogState(
                  () => validation = 'Every environment entry needs KEY=VALUE.',
                );
                return;
              }
              Navigator.pop(
                context,
                LaunchOptions(
                  directory: directory.text.trim(),
                  account: account.text.trim(),
                  backend: backend,
                  environment: env,
                  environmentFiles: nonEmptyLines(environmentFiles.text),
                  extraArguments: nonEmptyLines(arguments.text),
                  noWait: noWait,
                  readyTimeout: readyTimeout ?? 30,
                ),
              );
            },
            child: Text(fork ? 'Fork' : 'Start'),
          ),
        ],
      ),
    ),
  );
  directory.dispose();
  account.dispose();
  environment.dispose();
  environmentFiles.dispose();
  arguments.dispose();
  timeout.dispose();
  return result;
}

Future<SendOptions?> showSendDialog(BuildContext context) async {
  final target = TextEditingController(text: 'tag=');
  final prompt = TextEditingController();
  final filters = TextEditingController();
  final timeout = TextEditingController(text: '60');
  var selection = 'one', interrupt = false;
  String? validation;
  final result = await showDialog<SendOptions>(
    context: context,
    builder: (context) => StatefulBuilder(
      builder: (context, setDialogState) => AlertDialog(
        title: const Text('Send to panes'),
        content: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              TextField(
                controller: target,
                autocorrect: false,
                decoration: const InputDecoration(
                  labelText: 'Pane or primary filter',
                  hintText: '%42 or tag=worker',
                ),
              ),
              const SizedBox(height: 16),
              TextField(
                controller: prompt,
                minLines: 2,
                maxLines: 6,
                decoration: const InputDecoration(labelText: 'Prompt'),
              ),
              const SizedBox(height: 16),
              TextField(
                controller: filters,
                minLines: 1,
                maxLines: 4,
                autocorrect: false,
                decoration: const InputDecoration(
                  labelText: 'Additional filters (one key=value per line)',
                ),
              ),
              const SizedBox(height: 16),
              DropdownButtonFormField<String>(
                initialValue: selection,
                decoration: const InputDecoration(labelText: 'Selection'),
                items: const [
                  DropdownMenuItem(value: 'one', child: Text('Exactly one')),
                  DropdownMenuItem(value: 'any', child: Text('Any one')),
                  DropdownMenuItem(value: 'all', child: Text('All matches')),
                ],
                onChanged: (value) =>
                    setDialogState(() => selection = value ?? 'one'),
              ),
              const SizedBox(height: 16),
              TextField(
                controller: timeout,
                keyboardType: TextInputType.number,
                decoration: const InputDecoration(
                  labelText: 'Submit timeout (seconds)',
                ),
              ),
              SwitchListTile(
                contentPadding: EdgeInsets.zero,
                title: const Text('Interrupt before sending'),
                value: interrupt,
                onChanged: (value) => setDialogState(() => interrupt = value),
              ),
              if (validation != null)
                Text(
                  validation!,
                  style: TextStyle(color: Theme.of(context).colorScheme.error),
                ),
            ],
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () {
              final seconds = int.tryParse(timeout.text.trim());
              if (target.text.trim().isEmpty || prompt.text.trim().isEmpty) {
                setDialogState(
                  () => validation = 'A pane/filter and prompt are required.',
                );
                return;
              }
              if (seconds == null || seconds < 1) {
                setDialogState(
                  () => validation = 'Timeout must be at least 1 second.',
                );
                return;
              }
              Navigator.pop(
                context,
                SendOptions(
                  target: target.text.trim(),
                  prompt: prompt.text,
                  filters: nonEmptyLines(filters.text),
                  selection: selection,
                  timeout: seconds,
                  interrupt: interrupt,
                ),
              );
            },
            child: const Text('Send'),
          ),
        ],
      ),
    ),
  );
  target.dispose();
  prompt.dispose();
  filters.dispose();
  timeout.dispose();
  return result;
}

Future<List<String>?> showTagsDialog(
  BuildContext context,
  List<String> current,
) async {
  final controller = TextEditingController(text: current.join('\n'));
  String? validation;
  final result = await showDialog<List<String>>(
    context: context,
    builder: (context) => StatefulBuilder(
      builder: (context, setDialogState) => AlertDialog(
        title: const Text('Manage tags'),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            TextField(
              controller: controller,
              minLines: 3,
              maxLines: 8,
              autocorrect: false,
              decoration: const InputDecoration(
                labelText: 'One tag per line',
                helperText: 'Letters, numbers, underscore, and hyphen',
              ),
            ),
            if (validation != null)
              Padding(
                padding: const EdgeInsets.only(top: 12),
                child: Text(
                  validation!,
                  style: TextStyle(color: Theme.of(context).colorScheme.error),
                ),
              ),
          ],
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () {
              final tags = nonEmptyLines(controller.text).toSet().toList()
                ..sort();
              if (tags.any(
                (tag) => !RegExp(r'^[A-Za-z0-9_-]+$').hasMatch(tag),
              )) {
                setDialogState(
                  () => validation = 'One or more tags are invalid.',
                );
                return;
              }
              Navigator.pop(context, tags);
            },
            child: const Text('Save'),
          ),
        ],
      ),
    ),
  );
  controller.dispose();
  return result;
}
