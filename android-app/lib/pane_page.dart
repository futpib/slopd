import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';

import 'control.dart';
import 'dialogs.dart';
import 'models.dart';
import 'ui.dart';

class PanePage extends StatefulWidget {
  const PanePage({super.key, required this.api, required this.pane});

  final ControlConnection api;
  final PaneInfo pane;

  @override
  State<PanePage> createState() => _PanePageState();
}

class _PanePageState extends State<PanePage> with WidgetsBindingObserver {
  static const transcriptPageSize = 300;

  final prompt = TextEditingController();
  final scroll = ScrollController();
  late PaneInfo pane = widget.pane;
  List<TranscriptEntry> records = const [];
  List<_ConversationItem>? cachedItems;
  String error = '';
  bool loading = false,
      refreshing = false,
      refreshAfterCurrent = false,
      loadingEarlier = false,
      loadedEarlier = false,
      hasEarlier = true,
      sending = false,
      interruptFirst = false,
      active = true,
      openingLive = false;
  int sendTimeout = 60;
  Timer? timer;
  CommandStream? live;
  StreamSubscription<String>? liveLines, liveErrors;
  int liveGeneration = 0;
  int liveSerial = 0;
  final liveArrivals = <int, int>{};

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    widget.api.addListener(_connectionChanged);
    prompt.addListener(_promptChanged);
    _resume();
  }

  @override
  void dispose() {
    active = false;
    _stopLive();
    _stopPolling();
    widget.api.removeListener(_connectionChanged);
    WidgetsBinding.instance.removeObserver(this);
    prompt.removeListener(_promptChanged);
    prompt.dispose();
    scroll.dispose();
    super.dispose();
  }

  void _promptChanged() {
    if (mounted) setState(() {});
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    active = state == AppLifecycleState.resumed;
    if (active) {
      _resume();
    } else {
      _stopPolling();
      _stopLive();
    }
  }

  void _connectionChanged() {
    if (!mounted) return;
    setState(() {});
    if (widget.api.disconnected || widget.api.reconnecting) {
      _stopPolling();
      _stopLive();
    } else if (active) {
      _resume();
    }
  }

  void _resume() {
    if (!active || widget.api.disconnected || widget.api.reconnecting) return;
    timer ??= Timer.periodic(const Duration(seconds: 3), (_) {
      refresh(quiet: true);
      _startLive();
    });
    refresh(quiet: records.isNotEmpty);
    _startLive();
  }

  void _stopPolling() {
    timer?.cancel();
    timer = null;
  }

  Future<void> reconnect() async {
    setState(() => error = '');
    try {
      await widget.api.reconnect();
      _resume();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> refresh({bool quiet = false}) async {
    if (refreshing || widget.api.disconnected || widget.api.reconnecting) {
      return;
    }
    refreshing = true;
    final serialAtStart = liveSerial;
    if (!quiet && mounted) setState(() => loading = true);
    try {
      final results = await Future.wait([
        widget.api.command([
          'transcript',
          pane.id,
          '--limit',
          '$transcriptPageSize',
        ]),
        widget.api.command(['ps', '--json']),
      ]);
      final value = jsonDecode(results[0].stdout) as Map<String, dynamic>;
      final next = (value['records'] as List? ?? const [])
          .map(
            (record) =>
                TranscriptEntry.fromJson(record as Map<String, dynamic>),
          )
          .toList();
      final panes = (jsonDecode(results[1].stdout) as List).map(
        (value) => PaneInfo.fromJson(value as Map<String, dynamic>),
      );
      final updated = panes.where((value) => value.id == pane.id).firstOrNull;
      final shouldScroll = records.isEmpty || _nearBottom;
      if (mounted) {
        setState(() {
          final merged = _reconcileSnapshot(next, serialAtStart);
          final changed = !_sameRecords(records, merged);
          if (changed) {
            records = merged;
            cachedItems = null;
          }
          if (!loadedEarlier) hasEarlier = next.length >= transcriptPageSize;
          if (updated != null) pane = updated;
          error = updated == null ? 'This pane is no longer managed.' : '';
          if (changed && shouldScroll) _scrollToBottom();
        });
        liveArrivals.removeWhere((_, serial) => serial <= serialAtStart);
      }
    } on ConnectionLost {
      if (mounted) setState(() {});
    } catch (exception) {
      if (mounted) setState(() => error = 'Refresh failed: $exception');
    } finally {
      refreshing = false;
      if (!quiet && mounted) setState(() => loading = false);
      if (refreshAfterCurrent) {
        refreshAfterCurrent = false;
        if (mounted && active) unawaited(refresh(quiet: true));
      }
    }
  }

  Future<void> _refreshMetadata() async {
    if (refreshing || widget.api.disconnected || widget.api.reconnecting) {
      return;
    }
    try {
      final result = await widget.api.command(['ps', '--json']);
      final panes = (jsonDecode(result.stdout) as List).map(
        (value) => PaneInfo.fromJson(value as Map<String, dynamic>),
      );
      final updated = panes.where((value) => value.id == pane.id).firstOrNull;
      if (mounted) {
        setState(() {
          if (updated != null) pane = updated;
          error = updated == null ? 'This pane is no longer managed.' : '';
        });
      }
    } on ConnectionLost {
      if (mounted) setState(() {});
    } catch (exception) {
      if (mounted) setState(() => error = 'Refresh failed: $exception');
    }
  }

  Future<void> _startLive() async {
    if (live != null ||
        openingLive ||
        !active ||
        widget.api.disconnected ||
        widget.api.reconnecting) {
      return;
    }
    openingLive = true;
    final generation = ++liveGeneration;
    try {
      final stream = await widget.api.openStream([
        'listen',
        '--pane-id',
        pane.id,
        '--replay',
        '0',
      ]);
      if (!mounted || generation != liveGeneration || !active) {
        stream.close();
        return;
      }
      live = stream;
      liveLines = stream.lines.listen(_receiveLive);
      liveErrors = stream.errors.listen((line) {
        if (mounted && line.trim().isNotEmpty) {
          setState(() => error = line.trim());
        }
      });
      unawaited(
        stream.done
            .then<void>((_) {
              if (mounted && generation == liveGeneration) {
                _clearLive();
                unawaited(refresh(quiet: true));
              }
            })
            .catchError((Object exception) {
              if (mounted && generation == liveGeneration) {
                _clearLive();
                unawaited(refresh(quiet: true));
              }
            }),
      );
    } catch (exception) {
      if (mounted && generation == liveGeneration) {
        unawaited(refresh(quiet: true));
      }
    } finally {
      openingLive = false;
    }
  }

  void _receiveLive(String line) {
    if (!mounted || !active || line.trim().isEmpty) return;
    try {
      final value = jsonDecode(line) as Map<String, dynamic>;
      if (value['subscribed'] == true) {
        if (refreshing) {
          refreshAfterCurrent = true;
        } else {
          unawaited(refresh(quiet: true));
        }
        return;
      }
      if (value['event_type'] == 'ReplayEnd') {
        return;
      }
      final record = TranscriptEntry.fromJson(value);
      if (record.source != 'transcript') return;
      if (record.cursor != null) liveArrivals[record.cursor!] = ++liveSerial;
      final shouldScroll = _nearBottom;
      final next = _merge(records, [record]);
      if (!_sameRecords(records, next)) {
        setState(() {
          records = next;
          cachedItems = null;
          error = '';
        });
        if (shouldScroll) _scrollToBottom();
      }
    } catch (exception) {
      setState(() => error = 'Invalid live record: $exception');
    }
  }

  List<TranscriptEntry> _merge(
    Iterable<TranscriptEntry> first,
    Iterable<TranscriptEntry> second,
  ) {
    final result = <TranscriptEntry>[], cursors = <int>{};
    for (final record in [...first, ...second]) {
      final cursor = record.cursor;
      if (cursor == null || cursors.add(cursor)) result.add(record);
    }
    result.sort((a, b) => (a.cursor ?? 1 << 62).compareTo(b.cursor ?? 1 << 62));
    return result;
  }

  List<TranscriptEntry> _reconcileSnapshot(
    List<TranscriptEntry> snapshot,
    int serialAtStart,
  ) {
    if (snapshot.isEmpty) {
      return [
        for (final record in records)
          if (record.cursor != null &&
              (liveArrivals[record.cursor!] ?? 0) > serialAtStart)
            record,
      ];
    }
    final first = snapshot.first.cursor, last = snapshot.last.cursor;
    if (first == null || last == null) return _merge(snapshot, records);
    return _merge(
      [
        for (final record in records)
          if (record.cursor != null && record.cursor! < first) record,
        ...snapshot,
      ],
      [
        for (final record in records)
          if (record.cursor != null &&
              record.cursor! > last &&
              (liveArrivals[record.cursor!] ?? 0) > serialAtStart)
            record,
      ],
    );
  }

  bool _sameRecords(List<TranscriptEntry> first, List<TranscriptEntry> second) {
    if (first.length != second.length) return false;
    for (var i = 0; i < first.length; i++) {
      if (first[i].cursor != second[i].cursor ||
          first[i].type != second[i].type ||
          !_sameValue(first[i].payload, second[i].payload)) {
        return false;
      }
    }
    return true;
  }

  bool _sameValue(dynamic first, dynamic second) {
    if (identical(first, second)) return true;
    if (first is List && second is List) {
      if (first.length != second.length) return false;
      for (var i = 0; i < first.length; i++) {
        if (!_sameValue(first[i], second[i])) return false;
      }
      return true;
    }
    if (first is Map && second is Map) {
      if (first.length != second.length) return false;
      for (final key in first.keys) {
        if (!second.containsKey(key) || !_sameValue(first[key], second[key])) {
          return false;
        }
      }
      return true;
    }
    return first == second;
  }

  bool get _nearBottom =>
      !scroll.hasClients ||
      scroll.position.maxScrollExtent - scroll.position.pixels < 160;

  void _scrollToBottom() {
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted && scroll.hasClients) {
        scroll.animateTo(
          scroll.position.maxScrollExtent,
          duration: const Duration(milliseconds: 180),
          curve: Curves.easeOut,
        );
      }
    });
  }

  void _clearLive() {
    liveLines?.cancel();
    liveErrors?.cancel();
    liveLines = null;
    liveErrors = null;
    live = null;
  }

  void _stopLive() {
    liveGeneration++;
    final stream = live;
    _clearLive();
    stream?.close();
  }

  Future<void> loadEarlier() async {
    final cursor = records
        .map((record) => record.cursor)
        .whereType<int>()
        .firstOrNull;
    if (cursor == null || loadingEarlier || !hasEarlier) return;
    setState(() => loadingEarlier = true);
    final oldExtent = scroll.hasClients ? scroll.position.maxScrollExtent : 0.0;
    final oldOffset = scroll.hasClients ? scroll.position.pixels : 0.0;
    try {
      final result = await widget.api.command([
        'transcript',
        pane.id,
        '--before',
        '$cursor',
        '--limit',
        '100',
      ]);
      final value = jsonDecode(result.stdout) as Map<String, dynamic>;
      final earlier = (value['records'] as List? ?? const [])
          .map(
            (record) =>
                TranscriptEntry.fromJson(record as Map<String, dynamic>),
          )
          .toList();
      if (mounted) {
        setState(() {
          records = _merge(earlier, records);
          cachedItems = null;
          loadedEarlier = true;
          hasEarlier = earlier.length >= 100;
          error = '';
        });
        WidgetsBinding.instance.addPostFrameCallback((_) {
          if (mounted && scroll.hasClients) {
            final added = scroll.position.maxScrollExtent - oldExtent;
            scroll.jumpTo(
              (oldOffset + added).clamp(0, scroll.position.maxScrollExtent),
            );
          }
        });
      }
    } catch (exception) {
      if (mounted) {
        setState(() => error = 'Could not load earlier records: $exception');
      }
    } finally {
      if (mounted) setState(() => loadingEarlier = false);
    }
  }

  Future<void> send() async {
    final text = prompt.text;
    if (text.trim().isEmpty || sending) return;
    setState(() {
      sending = true;
      error = '';
    });
    try {
      await widget.api.command([
        'send',
        if (interruptFirst) '--interrupt',
        '--timeout',
        '$sendTimeout',
        pane.id,
        text,
      ], timeout: Duration(seconds: sendTimeout + 15));
      prompt.clear();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    } finally {
      if (mounted) setState(() => sending = false);
    }
  }

  Future<void> forkPane() async {
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

  Future<void> manageTags() async {
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
      await _refreshMetadata();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> interrupt() async {
    try {
      await widget.api.command(['interrupt', pane.id]);
      await _refreshMetadata();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> kill() async {
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
      if (mounted) Navigator.pop(context);
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> setSendTimeout() async {
    final controller = TextEditingController(text: '$sendTimeout');
    final value = await showDialog<int>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Send timeout'),
        content: TextField(
          controller: controller,
          keyboardType: TextInputType.number,
          decoration: const InputDecoration(labelText: 'Seconds'),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () {
              final seconds = int.tryParse(controller.text.trim());
              if (seconds != null && seconds > 0) {
                Navigator.pop(context, seconds);
              }
            },
            child: const Text('Save'),
          ),
        ],
      ),
    );
    controller.dispose();
    if (value != null && mounted) setState(() => sendTimeout = value);
  }

  Future<void> showPaneDetails() => showModalBottomSheet<void>(
    context: context,
    isScrollControlled: true,
    showDragHandle: true,
    builder: (context) => SafeArea(
      child: SingleChildScrollView(
        padding: const EdgeInsets.fromLTRB(24, 0, 24, 24),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(pane.title, style: Theme.of(context).textTheme.titleLarge),
            const SizedBox(height: 8),
            Wrap(
              spacing: 8,
              runSpacing: 8,
              children: [
                Chip(label: Text(_words(pane.detailedState))),
                Chip(label: Text('${pane.backend}/${pane.account}')),
                for (final tag in pane.tags) Chip(label: Text(tag)),
              ],
            ),
            const SizedBox(height: 20),
            _Detail('Working directory', pane.directory),
            _Detail(
              'Session',
              pane.sessionId.isEmpty ? 'Unknown' : pane.sessionId,
            ),
            _Detail(
              'Parent pane',
              pane.parentPaneId.isEmpty ? 'None' : pane.parentPaneId,
            ),
            _Detail('Created', formatAge(pane.createdAt)),
            _Detail('Last active', formatAge(pane.lastActive)),
            _Detail(
              'Transcript',
              pane.transcriptPath.isEmpty ? 'Unknown' : pane.transcriptPath,
            ),
          ],
        ),
      ),
    ),
  );

  @override
  Widget build(BuildContext context) {
    final items = cachedItems ??= _conversationItems(records);
    final disconnected = widget.api.disconnected || widget.api.reconnecting;
    return Scaffold(
      appBar: AppBar(
        titleSpacing: 4,
        title: InkWell(
          borderRadius: BorderRadius.circular(8),
          onTap: showPaneDetails,
          child: Padding(
            padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  pane.title,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: Theme.of(context).textTheme.titleMedium,
                ),
                Text(
                  '${pane.id} · ${_words(pane.detailedState)}',
                  style: Theme.of(context).textTheme.labelSmall?.copyWith(
                    color: Theme.of(context).colorScheme.onSurfaceVariant,
                  ),
                ),
              ],
            ),
          ),
        ),
        actions: [
          IconButton(
            tooltip: 'Refresh chat',
            onPressed: loading
                ? null
                : () {
                    refresh();
                    _startLive();
                  },
            icon: const Icon(Icons.refresh),
          ),
          PopupMenuButton<String>(
            onSelected: (value) => switch (value) {
              'details' => showPaneDetails(),
              'fork' => forkPane(),
              'tags' => manageTags(),
              'interrupt' => interrupt(),
              'kill' => kill(),
              _ => Future<void>.value(),
            },
            itemBuilder: (_) => const [
              PopupMenuItem(value: 'details', child: Text('Agent details')),
              PopupMenuItem(value: 'fork', child: Text('Fork')),
              PopupMenuItem(value: 'tags', child: Text('Manage tags')),
              PopupMenuItem(value: 'interrupt', child: Text('Interrupt')),
              PopupMenuItem(value: 'kill', child: Text('Kill')),
            ],
          ),
        ],
      ),
      body: Column(
        children: [
          if (disconnected)
            MaterialBanner(
              content: Text(
                widget.api.reconnecting ? 'Reconnecting…' : 'Connection lost',
              ),
              actions: [
                if (!widget.api.reconnecting)
                  TextButton(
                    onPressed: reconnect,
                    child: const Text('Reconnect'),
                  ),
              ],
            ),
          if (error.isNotEmpty)
            Padding(
              padding: const EdgeInsets.fromLTRB(12, 8, 12, 0),
              child: ErrorBox(error),
            ),
          Expanded(
            child: loading && records.isEmpty
                ? const Center(child: CircularProgressIndicator())
                : records.isEmpty
                ? const EmptyState(
                    Icons.chat_bubble_outline,
                    'No messages yet',
                    'Send a message to start this conversation.',
                  )
                : ListView.builder(
                    controller: scroll,
                    padding: const EdgeInsets.fromLTRB(12, 16, 12, 12),
                    itemCount: items.length + (hasEarlier ? 1 : 0),
                    itemBuilder: (context, index) {
                      if (hasEarlier && index == 0) {
                        return Center(
                          child: TextButton.icon(
                            onPressed: loadingEarlier ? null : loadEarlier,
                            icon: loadingEarlier
                                ? const SizedBox.square(
                                    dimension: 16,
                                    child: CircularProgressIndicator(
                                      strokeWidth: 2,
                                    ),
                                  )
                                : const Icon(Icons.history),
                            label: const Text('Earlier messages'),
                          ),
                        );
                      }
                      final item = items[index - (hasEarlier ? 1 : 0)];
                      return _ConversationItemView(item: item);
                    },
                  ),
          ),
          SafeArea(
            top: false,
            child: Material(
              color: Theme.of(context).colorScheme.surfaceContainer,
              child: Padding(
                padding: const EdgeInsets.fromLTRB(12, 10, 8, 8),
                child: Column(
                  mainAxisSize: MainAxisSize.min,
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Row(
                      crossAxisAlignment: CrossAxisAlignment.end,
                      children: [
                        Expanded(
                          child: TextField(
                            controller: prompt,
                            minLines: 1,
                            maxLines: 6,
                            enabled: !sending && !widget.api.disconnected,
                            textCapitalization: TextCapitalization.sentences,
                            decoration: const InputDecoration(
                              hintText: 'Message agent',
                              filled: true,
                            ),
                          ),
                        ),
                        PopupMenuButton<String>(
                          tooltip: 'Message options',
                          enabled: !sending,
                          icon: const Icon(Icons.tune),
                          onSelected: (value) {
                            if (value == 'interrupt') {
                              setState(() => interruptFirst = !interruptFirst);
                            } else if (value == 'timeout') {
                              setSendTimeout();
                            }
                          },
                          itemBuilder: (_) => [
                            CheckedPopupMenuItem(
                              value: 'interrupt',
                              checked: interruptFirst,
                              child: const Text('Interrupt before sending'),
                            ),
                            PopupMenuItem(
                              value: 'timeout',
                              child: Text('Send timeout · ${sendTimeout}s'),
                            ),
                          ],
                        ),
                        IconButton.filled(
                          tooltip: 'Send message',
                          onPressed:
                              sending ||
                                  widget.api.disconnected ||
                                  prompt.text.trim().isEmpty
                              ? null
                              : send,
                          icon: sending
                              ? const SizedBox.square(
                                  dimension: 20,
                                  child: CircularProgressIndicator(
                                    strokeWidth: 2,
                                  ),
                                )
                              : const Icon(Icons.arrow_upward),
                        ),
                      ],
                    ),
                    Padding(
                      padding: const EdgeInsets.only(left: 12, top: 4),
                      child: Text(
                        'Local commands start with ! · ${sendTimeout}s'
                        '${interruptFirst ? ' · interrupt first' : ''}',
                        style: Theme.of(context).textTheme.labelSmall?.copyWith(
                          color: Theme.of(context).colorScheme.onSurfaceVariant,
                        ),
                      ),
                    ),
                  ],
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _Detail extends StatelessWidget {
  const _Detail(this.label, this.value);

  final String label, value;

  @override
  Widget build(BuildContext context) => Padding(
    padding: const EdgeInsets.only(bottom: 16),
    child: Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(label, style: Theme.of(context).textTheme.labelMedium),
        const SizedBox(height: 4),
        SelectableText(value.isEmpty ? 'Unknown' : value),
      ],
    ),
  );
}

class _ConversationItem {
  _ConversationItem(this.kind, TranscriptEntry record) : records = [record];

  final TranscriptKind kind;
  final List<TranscriptEntry> records;

  bool get chunks =>
      records.every((record) => record.type.toLowerCase().contains('chunk'));

  String get message => records
      .map((record) => record.messageText ?? record.text)
      .join(chunks ? '' : '\n\n');
}

List<_ConversationItem> _conversationItems(List<TranscriptEntry> records) {
  final items = <_ConversationItem>[];
  for (final record in records) {
    final kind = record.kind;
    final previous = items.lastOrNull;
    final combineActivity =
        kind == TranscriptKind.activity &&
        previous?.kind == TranscriptKind.activity;
    final combineChunks =
        (kind == TranscriptKind.user || kind == TranscriptKind.assistant) &&
        previous?.kind == kind &&
        previous!.chunks &&
        record.type.toLowerCase().contains('chunk');
    if (combineActivity || combineChunks) {
      previous!.records.add(record);
    } else {
      items.add(_ConversationItem(kind, record));
    }
  }
  return items;
}

class _ConversationItemView extends StatelessWidget {
  const _ConversationItemView({required this.item});

  final _ConversationItem item;

  @override
  Widget build(BuildContext context) => switch (item.kind) {
    TranscriptKind.user => _MessageBubble(item: item, user: true),
    TranscriptKind.assistant => _MessageBubble(item: item, user: false),
    TranscriptKind.error => _FailureCard(item.records.first),
    TranscriptKind.activity => _ActivityCard(item.records),
  };
}

class _MessageBubble extends StatelessWidget {
  const _MessageBubble({required this.item, required this.user});

  final _ConversationItem item;
  final bool user;

  @override
  Widget build(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    final timestamp = _shortTimestamp(item.records.last.timestamp);
    return Align(
      alignment: user ? Alignment.centerRight : Alignment.centerLeft,
      child: ConstrainedBox(
        constraints: BoxConstraints(
          maxWidth: MediaQuery.sizeOf(context).width * 0.86,
        ),
        child: Container(
          margin: const EdgeInsets.only(bottom: 12),
          padding: const EdgeInsets.fromLTRB(14, 10, 14, 12),
          decoration: BoxDecoration(
            color: user ? colors.primaryContainer : colors.surfaceContainerHigh,
            borderRadius: BorderRadius.only(
              topLeft: const Radius.circular(18),
              topRight: const Radius.circular(18),
              bottomLeft: Radius.circular(user ? 18 : 4),
              bottomRight: Radius.circular(user ? 4 : 18),
            ),
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Icon(
                    user ? Icons.person_outline : Icons.smart_toy_outlined,
                    size: 16,
                    color: colors.onSurfaceVariant,
                  ),
                  const SizedBox(width: 6),
                  Text(
                    user ? 'You' : 'Agent',
                    style: Theme.of(context).textTheme.labelMedium,
                  ),
                  if (timestamp.isNotEmpty) ...[
                    const SizedBox(width: 8),
                    Text(
                      timestamp,
                      style: Theme.of(context).textTheme.labelSmall?.copyWith(
                        color: colors.onSurfaceVariant,
                      ),
                    ),
                  ],
                ],
              ),
              const SizedBox(height: 6),
              SelectableText(item.message),
            ],
          ),
        ),
      ),
    );
  }
}

class _ActivityCard extends StatelessWidget {
  const _ActivityCard(this.records);

  final List<TranscriptEntry> records;

  @override
  Widget build(BuildContext context) {
    final labels = records.map((record) => record.activityLabel).toList();
    final distinct = labels.toSet().take(3).join(' · ');
    return Card(
      color: Theme.of(context).colorScheme.surfaceContainerLow,
      margin: const EdgeInsets.fromLTRB(20, 0, 20, 10),
      child: ExpansionTile(
        dense: true,
        leading: const Icon(Icons.auto_awesome_outlined, size: 20),
        title: Text(
          records.length == 1 ? labels.first : '${records.length} steps',
        ),
        subtitle: records.length == 1 ? null : Text(distinct),
        childrenPadding: const EdgeInsets.fromLTRB(16, 0, 16, 12),
        children: [
          for (final record in records)
            Padding(
              padding: const EdgeInsets.only(top: 10),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  Text(
                    record.activityLabel,
                    style: Theme.of(context).textTheme.labelMedium,
                  ),
                  const SizedBox(height: 4),
                  SelectableText(
                    _limited(record.activityDetail),
                    style: mono.copyWith(fontSize: 11),
                  ),
                ],
              ),
            ),
        ],
      ),
    );
  }
}

class _FailureCard extends StatelessWidget {
  const _FailureCard(this.record);

  final TranscriptEntry record;

  @override
  Widget build(BuildContext context) => Card(
    color: Theme.of(context).colorScheme.errorContainer,
    margin: const EdgeInsets.only(bottom: 12),
    child: ListTile(
      leading: const Icon(Icons.error_outline),
      title: const Text('Agent error'),
      subtitle: SelectableText(_limited(record.text, limit: 2000)),
    ),
  );
}

String _limited(String value, {int limit = 4000}) {
  final text = value.trim();
  if (text.length <= limit) return text;
  return '${text.substring(0, limit)}\n… ${text.length - limit} more characters';
}

String _shortTimestamp(String value) {
  if (value.isEmpty) return '';
  final parsed = DateTime.tryParse(value)?.toLocal();
  if (parsed == null) return value;
  final hour = '${parsed.hour}'.padLeft(2, '0');
  final minute = '${parsed.minute}'.padLeft(2, '0');
  return '$hour:$minute';
}

String _words(String value) => value
    .replaceAllMapped(
      RegExp(r'([a-z0-9])([A-Z])'),
      (match) => '${match.group(1)} ${match.group(2)}',
    )
    .replaceAll('_', ' ')
    .trim();
