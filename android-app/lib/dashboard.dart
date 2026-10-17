import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';

import 'control.dart';
import 'dialogs.dart';
import 'events_page.dart';
import 'graveyard_page.dart';
import 'models.dart';
import 'pane_page.dart';
import 'ui.dart';

export 'models.dart';

class Dashboard extends StatefulWidget {
  const Dashboard({
    super.key,
    required this.api,
    required this.name,
    required this.connection,
  });

  final ControlConnection api;
  final String name, connection;

  @override
  State<Dashboard> createState() => _DashboardState();
}

class _DashboardState extends State<Dashboard> with WidgetsBindingObserver {
  final search = TextEditingController();
  final paneViewStates = <String, PaneViewState>{};
  List<PaneInfo> panes = const [];
  DaemonStatus? status;
  ClientInfo? info;
  String error = '', stateFilter = 'all', tagFilter = 'all', sort = 'activity';
  bool loading = false, refreshing = false, active = true, childOpen = false;
  Timer? timer;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    widget.api.addListener(_connectionChanged);
    search.addListener(_searchChanged);
    _resume();
  }

  @override
  void dispose() {
    timer?.cancel();
    search.removeListener(_searchChanged);
    search.dispose();
    widget.api.removeListener(_connectionChanged);
    WidgetsBinding.instance.removeObserver(this);
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    active = state == AppLifecycleState.resumed;
    if (active) {
      _resume();
    } else {
      _stopPolling();
    }
  }

  void _searchChanged() => setState(() {});

  void _connectionChanged() {
    if (!mounted) return;
    setState(() {});
    if (widget.api.disconnected || widget.api.reconnecting) {
      _stopPolling();
    } else {
      _resume();
    }
  }

  void _resume() {
    if (!active ||
        childOpen ||
        widget.api.disconnected ||
        widget.api.reconnecting) {
      return;
    }
    timer ??= Timer.periodic(
      const Duration(seconds: 3),
      (_) => refresh(quiet: true),
    );
    refresh(quiet: panes.isNotEmpty);
  }

  void _stopPolling() {
    timer?.cancel();
    timer = null;
  }

  Future<void> _open(Widget page) async {
    childOpen = true;
    _stopPolling();
    try {
      await Navigator.of(
        context,
      ).push<void>(MaterialPageRoute(builder: (_) => page));
    } finally {
      childOpen = false;
      if (mounted) _resume();
    }
  }

  Future<void> refresh({bool quiet = false}) async {
    if (refreshing ||
        childOpen ||
        widget.api.disconnected ||
        widget.api.reconnecting) {
      return;
    }
    refreshing = true;
    if (!quiet && mounted) setState(() => loading = true);
    try {
      final results = await Future.wait([
        widget.api.command(['ps', '--json']),
        widget.api.command(['status', '--json']),
        if (info == null) widget.api.command(['info', '--json']),
      ]);
      final values = (jsonDecode(results[0].stdout) as List)
          .map((value) => PaneInfo.fromJson(value as Map<String, dynamic>))
          .toList();
      final nextStatus = DaemonStatus.fromJson(
        jsonDecode(results[1].stdout) as Map<String, dynamic>,
      );
      final nextInfo = results.length > 2
          ? ClientInfo.fromJson(
              jsonDecode(results[2].stdout) as Map<String, dynamic>,
            )
          : info;
      if (mounted) {
        setState(() {
          panes = values;
          status = nextStatus;
          info = nextInfo;
          error = '';
        });
      }
    } on ConnectionLost {
      if (mounted) setState(() {});
    } catch (exception) {
      if (mounted) setState(() => error = 'Refresh failed: $exception');
    } finally {
      refreshing = false;
      if (!quiet && mounted) setState(() => loading = false);
    }
  }

  Future<void> reconnect() async {
    setState(() => error = '');
    try {
      await widget.api.reconnect();
      info = null;
      _resume();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> runPane() async {
    final options = await showLaunchDialog(context);
    if (options == null || !mounted) return;
    setState(() {
      loading = true;
      error = '';
    });
    try {
      final result = await widget.api.command(
        options.runArguments(),
        timeout: Duration(
          seconds: options.noWait ? 45 : options.readyTimeout + 20,
        ),
      );
      await refresh(quiet: true);
      if (mounted) {
        final paneId = result.stdout.trim();
        ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(
            content: Text(options.noWait ? 'Created $paneId' : 'Ready $paneId'),
          ),
        );
      }
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    } finally {
      if (mounted) setState(() => loading = false);
    }
  }

  Future<void> sendFiltered() async {
    final options = await showSendDialog(context);
    if (options == null || !mounted) return;
    setState(() => loading = true);
    try {
      final result = await widget.api.command(
        options.arguments(),
        timeout: Duration(seconds: options.timeout + 15),
      );
      if (mounted) {
        final ids = result.stdout
            .trim()
            .split('\n')
            .where((value) => value.isNotEmpty);
        ScaffoldMessenger.of(
          context,
        ).showSnackBar(SnackBar(content: Text('Sent to ${ids.join(', ')}')));
      }
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    } finally {
      if (mounted) setState(() => loading = false);
    }
  }

  Future<void> forkPane(PaneInfo pane) async {
    final options = await showLaunchDialog(context, fork: true);
    if (options == null || !mounted) return;
    setState(() => loading = true);
    try {
      final result = await widget.api.command(
        options.forkArguments(pane.id),
        timeout: Duration(
          seconds: options.noWait ? 45 : options.readyTimeout + 20,
        ),
      );
      await refresh(quiet: true);
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(content: Text('Forked ${result.stdout.trim()}')),
        );
      }
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    } finally {
      if (mounted) setState(() => loading = false);
    }
  }

  Future<void> manageTags(PaneInfo pane) async {
    final next = await showTagsDialog(context, pane.tags);
    if (next == null || !mounted) return;
    final current = pane.tags.toSet(), desired = next.toSet();
    try {
      for (final tag in current.difference(desired)) {
        await widget.api.command(['untag', pane.id, tag]);
      }
      for (final tag in desired.difference(current)) {
        await widget.api.command(['tag', pane.id, tag]);
      }
      await refresh();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> interrupt(PaneInfo pane) async {
    try {
      await widget.api.command(['interrupt', pane.id]);
      await refresh();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> kill(PaneInfo pane) async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Kill ${pane.id}?'),
        content: Text(pane.title),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Kill'),
          ),
        ],
      ),
    );
    if (confirmed != true) return;
    try {
      await widget.api.command(['kill', pane.id]);
      await refresh();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> backup() async => _mutation(['backup']);

  Future<void> restore() async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Restore checkpoint?'),
        content: const Text(
          'Sessions already running are skipped. Missing retained sessions are re-spawned.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Restore'),
          ),
        ],
      ),
    );
    if (confirmed == true) {
      await _mutation(['restore'], timeout: const Duration(seconds: 90));
    }
  }

  Future<void> _mutation(
    List<String> arguments, {
    Duration timeout = const Duration(seconds: 45),
  }) async {
    setState(() => loading = true);
    try {
      final result = await widget.api.command(arguments, timeout: timeout);
      await refresh(quiet: true);
      if (mounted) {
        ScaffoldMessenger.of(
          context,
        ).showSnackBar(SnackBar(content: Text(result.stdout.trim())));
      }
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    } finally {
      if (mounted) setState(() => loading = false);
    }
  }

  Set<String> get tags => panes.expand((pane) => pane.tags).toSet();

  List<PaneInfo> get visiblePanes {
    final result = panes.where((pane) {
      return pane.matches(search.text) &&
          (stateFilter == 'all' || pane.state == stateFilter) &&
          (tagFilter == 'all' || pane.tags.contains(tagFilter));
    }).toList();
    result.sort(switch (sort) {
      'title' => (a, b) => a.title.toLowerCase().compareTo(
        b.title.toLowerCase(),
      ),
      'created' => (a, b) => b.createdAt.compareTo(a.createdAt),
      'state' => (a, b) {
        final state = a.state.compareTo(b.state);
        return state == 0 ? b.lastActive.compareTo(a.lastActive) : state;
      },
      _ => (a, b) => b.lastActive.compareTo(a.lastActive),
    });
    return result;
  }

  Color stateColor(BuildContext context, String state) => switch (state) {
    'ready' => Colors.green,
    'busy' => Theme.of(context).colorScheme.primary,
    'awaiting_input' => Colors.amber,
    _ => Theme.of(context).colorScheme.onSurfaceVariant,
  };

  Widget statusCard(BuildContext context) {
    final daemon = status, client = info;
    if (daemon == null) return const SizedBox.shrink();
    return Card(
      child: ExpansionTile(
        leading: const Icon(Icons.check_circle_outline, color: Colors.green),
        title: const Text('slopd service online'),
        subtitle: Text(
          'Uptime ${formatDuration(daemon.uptimeSeconds)} · '
          '${daemon.subscribers} subscriber${daemon.subscribers == 1 ? '' : 's'}',
        ),
        childrenPadding: const EdgeInsets.fromLTRB(16, 0, 16, 16),
        expandedCrossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SelectableText(
            'Config generation: ${daemon.configGeneration}\n'
            'Pending restore: ${daemon.pendingRestore ?? 0}\n'
            'slopctl: ${client == null ? 'unknown' : '${client.version} (${client.commit})'}\n'
            'Config: ${client?.configPath ?? 'unknown'}\n'
            'Socket: ${client?.socketPath ?? 'unknown'}',
            style: mono.copyWith(fontSize: 11),
          ),
          if (client != null && client.capabilities.isNotEmpty) ...[
            const SizedBox(height: 12),
            Text('Capabilities', style: Theme.of(context).textTheme.labelLarge),
            const SizedBox(height: 6),
            Wrap(
              spacing: 6,
              runSpacing: 4,
              children: [
                for (final capability in client.capabilities)
                  Chip(label: Text(capability.replaceAll('_', ' '))),
              ],
            ),
          ],
        ],
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final shown = visiblePanes;
    final availableTags = tags.toList()..sort();
    if (tagFilter != 'all' && !availableTags.contains(tagFilter)) {
      tagFilter = 'all';
    }
    return Scaffold(
      appBar: AppBar(
        title: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(widget.name),
            Text(
              widget.connection,
              style: Theme.of(context).textTheme.labelSmall,
            ),
          ],
        ),
        actions: [
          PopupMenuButton<String>(
            enabled:
                !loading &&
                !widget.api.disconnected &&
                !widget.api.reconnecting,
            onSelected: (value) => switch (value) {
              'send' => sendFiltered(),
              'events' => _open(EventsPage(api: widget.api)),
              'backup' => backup(),
              'restore' => restore(),
              'graveyard' => _open(GraveyardPage(api: widget.api)),
              'reconnect' => reconnect(),
              _ => Future<void>.value(),
            },
            itemBuilder: (_) => const [
              PopupMenuItem(value: 'send', child: Text('Send to panes')),
              PopupMenuItem(value: 'events', child: Text('Events and waits')),
              PopupMenuItem(value: 'backup', child: Text('Back up now')),
              PopupMenuItem(
                value: 'restore',
                child: Text('Restore checkpoint'),
              ),
              PopupMenuItem(
                value: 'graveyard',
                child: Text('Lifecycle graveyard'),
              ),
              PopupMenuItem(value: 'reconnect', child: Text('Reconnect')),
            ],
          ),
          IconButton(
            tooltip: 'Refresh',
            onPressed: loading ? null : refresh,
            icon: const Icon(Icons.refresh),
          ),
        ],
      ),
      floatingActionButton: FloatingActionButton.extended(
        onPressed: loading || widget.api.disconnected ? null : runPane,
        icon: const Icon(Icons.add),
        label: const Text('Start agent'),
      ),
      body: RefreshIndicator(
        onRefresh: refresh,
        child: ListView(
          physics: const AlwaysScrollableScrollPhysics(),
          padding: const EdgeInsets.fromLTRB(20, 8, 20, 100),
          children: [
            if (widget.api.disconnected || widget.api.reconnecting)
              Card(
                color: Theme.of(context).colorScheme.errorContainer,
                child: ListTile(
                  leading: const Icon(Icons.link_off),
                  title: Text(
                    widget.api.reconnecting
                        ? 'Reconnecting…'
                        : 'Connection lost',
                  ),
                  trailing: widget.api.reconnecting
                      ? const SizedBox.square(
                          dimension: 22,
                          child: CircularProgressIndicator(strokeWidth: 2),
                        )
                      : TextButton(
                          onPressed: reconnect,
                          child: const Text('Reconnect'),
                        ),
                ),
              ),
            if (error.isNotEmpty) ErrorBox(error),
            statusCard(context),
            const SizedBox(height: 8),
            TextField(
              controller: search,
              decoration: InputDecoration(
                labelText: 'Search agents',
                prefixIcon: const Icon(Icons.search),
                suffixIcon: search.text.isEmpty
                    ? null
                    : IconButton(
                        tooltip: 'Clear search',
                        onPressed: search.clear,
                        icon: const Icon(Icons.clear),
                      ),
              ),
            ),
            const SizedBox(height: 12),
            Wrap(
              spacing: 12,
              runSpacing: 8,
              children: [
                DropdownButton<String>(
                  value: stateFilter,
                  items: const [
                    DropdownMenuItem(value: 'all', child: Text('All states')),
                    DropdownMenuItem(value: 'ready', child: Text('Ready')),
                    DropdownMenuItem(value: 'busy', child: Text('Busy')),
                    DropdownMenuItem(
                      value: 'awaiting_input',
                      child: Text('Awaiting input'),
                    ),
                    DropdownMenuItem(
                      value: 'booting_up',
                      child: Text('Booting'),
                    ),
                  ],
                  onChanged: (value) =>
                      setState(() => stateFilter = value ?? 'all'),
                ),
                DropdownButton<String>(
                  value: tagFilter,
                  items: [
                    const DropdownMenuItem(
                      value: 'all',
                      child: Text('All tags'),
                    ),
                    for (final tag in availableTags)
                      DropdownMenuItem(value: tag, child: Text(tag)),
                  ],
                  onChanged: (value) =>
                      setState(() => tagFilter = value ?? 'all'),
                ),
                DropdownButton<String>(
                  value: sort,
                  items: const [
                    DropdownMenuItem(
                      value: 'activity',
                      child: Text('Recent activity'),
                    ),
                    DropdownMenuItem(
                      value: 'created',
                      child: Text('Newest created'),
                    ),
                    DropdownMenuItem(value: 'title', child: Text('Title')),
                    DropdownMenuItem(value: 'state', child: Text('State')),
                  ],
                  onChanged: (value) =>
                      setState(() => sort = value ?? 'activity'),
                ),
              ],
            ),
            SectionHeading('Agents · ${shown.length}/${panes.length}'),
            if (loading && panes.isEmpty)
              const Center(
                child: Padding(
                  padding: EdgeInsets.all(48),
                  child: CircularProgressIndicator(),
                ),
              )
            else if (shown.isEmpty)
              EmptyState(
                Icons.smart_toy_outlined,
                panes.isEmpty ? 'No managed agents' : 'No matching agents',
                panes.isEmpty
                    ? 'Start an agent on this server to manage it here.'
                    : 'Change the search or filters.',
              ),
            for (final pane in shown)
              Card(
                child: ListTile(
                  contentPadding: const EdgeInsets.fromLTRB(16, 10, 8, 10),
                  leading: Icon(
                    pane.state == 'busy'
                        ? Icons.pending_outlined
                        : Icons.smart_toy_outlined,
                    color: stateColor(context, pane.state),
                  ),
                  title: Text(
                    pane.title,
                    maxLines: 2,
                    overflow: TextOverflow.ellipsis,
                  ),
                  subtitle: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(
                        '${pane.id} · ${pane.backend}/${pane.account}\n'
                        '${pane.detailedState}${pane.directory.isEmpty ? '' : ' · ${pane.directory}'}\n'
                        'Active ${formatAge(pane.lastActive)}'
                        '${pane.sessionId.isEmpty ? '' : ' · ${pane.sessionId}'}',
                        maxLines: 4,
                        overflow: TextOverflow.ellipsis,
                      ),
                      if (pane.tags.isNotEmpty)
                        Wrap(
                          spacing: 4,
                          children: [
                            for (final tag in pane.tags)
                              Chip(
                                visualDensity: VisualDensity.compact,
                                label: Text(tag),
                              ),
                          ],
                        ),
                    ],
                  ),
                  onTap: () {
                    final key = '${pane.id}:${pane.sessionId}';
                    _open(
                      PanePage(
                        api: widget.api,
                        pane: pane,
                        viewState: paneViewStates.putIfAbsent(
                          key,
                          PaneViewState.new,
                        ),
                      ),
                    );
                  },
                  trailing: PopupMenuButton<String>(
                    onSelected: (value) => switch (value) {
                      'fork' => forkPane(pane),
                      'tags' => manageTags(pane),
                      'interrupt' => interrupt(pane),
                      'kill' => kill(pane),
                      _ => Future<void>.value(),
                    },
                    itemBuilder: (_) => const [
                      PopupMenuItem(value: 'fork', child: Text('Fork')),
                      PopupMenuItem(value: 'tags', child: Text('Manage tags')),
                      PopupMenuItem(
                        value: 'interrupt',
                        child: Text('Interrupt'),
                      ),
                      PopupMenuItem(value: 'kill', child: Text('Kill')),
                    ],
                  ),
                ),
              ),
          ],
        ),
      ),
    );
  }
}
