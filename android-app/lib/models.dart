import 'dart:convert';

class PaneInfo {
  const PaneInfo({
    required this.id,
    required this.title,
    required this.state,
    required this.detailedState,
    required this.directory,
    required this.transcriptPath,
    required this.account,
    required this.backend,
    required this.sessionId,
    required this.parentPaneId,
    required this.tags,
    required this.createdAt,
    required this.lastActive,
  });

  final String id,
      title,
      state,
      detailedState,
      directory,
      transcriptPath,
      account,
      backend,
      sessionId,
      parentPaneId;
  final List<String> tags;
  final int createdAt, lastActive;

  factory PaneInfo.fromJson(Map<String, dynamic> value) => PaneInfo(
    id: value['pane_id'] as String,
    title: (value['pane_title'] as String?)?.trim().isNotEmpty == true
        ? value['pane_title'] as String
        : value['pane_id'] as String,
    state: value['state'] as String? ?? 'unknown',
    detailedState: value['detailed_state'] as String? ?? 'unknown',
    directory: value['working_dir'] as String? ?? '',
    transcriptPath: value['transcript_path'] as String? ?? '',
    account: value['account'] as String? ?? '',
    backend: value['backend'] as String? ?? '',
    sessionId: value['session_id'] as String? ?? '',
    parentPaneId: value['parent_pane_id'] as String? ?? '',
    tags: (value['tags'] as List? ?? const []).cast<String>(),
    createdAt: (value['created_at'] as num?)?.toInt() ?? 0,
    lastActive: (value['last_active'] as num?)?.toInt() ?? 0,
  );

  bool matches(String query) {
    final needle = query.trim().toLowerCase();
    if (needle.isEmpty) return true;
    return [
      id,
      title,
      state,
      detailedState,
      directory,
      account,
      backend,
      sessionId,
      parentPaneId,
      ...tags,
    ].any((value) => value.toLowerCase().contains(needle));
  }
}

enum TranscriptKind { user, assistant, activity, error }

class TranscriptEntry {
  const TranscriptEntry({
    required this.type,
    required this.source,
    required this.payload,
    required this.cursor,
  });

  final String type, source;
  final dynamic payload;
  final int? cursor;

  factory TranscriptEntry.fromJson(Map<String, dynamic> value) =>
      TranscriptEntry(
        type: value['event_type'] as String? ?? 'record',
        source: value['source'] as String? ?? 'transcript',
        payload: value['payload'],
        cursor: (value['cursor'] as num?)?.toInt(),
      );

  String get text => messageText ?? _readable(payload) ?? _pretty(payload);

  TranscriptKind get kind {
    final normalized = type.toLowerCase().replaceAll(RegExp(r'[^a-z]'), '');
    if (normalized.contains('fail') || normalized.contains('error')) {
      return TranscriptKind.error;
    }
    if ({'user', 'usermessage', 'usermessagechunk'}.contains(normalized)) {
      return TranscriptKind.user;
    }
    if ({
      'assistant',
      'agentmessage',
      'agentmessagechunk',
    }.contains(normalized)) {
      return messageText == null
          ? TranscriptKind.activity
          : TranscriptKind.assistant;
    }
    return TranscriptKind.activity;
  }

  String? get messageText {
    final value = payload;
    if (value is String && value.trim().isNotEmpty) return value;
    if (value is! Map) return null;

    final direct = _textContent(value['text']);
    if (direct != null) return direct;
    final message = value['message'];
    if (message is Map) {
      final text =
          _textContent(message['content']) ?? _textContent(message['text']);
      if (text != null) return text;
    }
    final parts = _textContent(value['parts']);
    if (parts != null) return parts;

    final update =
        _mapAt(value, const ['params', 'update']) ??
        _mapAt(value, const ['update']);
    return update == null ? null : _textContent(update['content']);
  }

  String get activityLabel {
    final normalized = type.toLowerCase().replaceAll(RegExp(r'[^a-z]'), '');
    if (kind == TranscriptKind.error) return 'Agent error';
    if (normalized == 'reasoning' || normalized.contains('thought')) {
      return 'Thinking';
    }
    if (normalized == 'commandexecution') return 'Running command';
    if (normalized == 'filechange') return 'Editing files';
    if (normalized == 'websearch') return 'Searching the web';
    if (normalized == 'plan') return 'Updating plan';
    if (normalized.contains('toolresult')) return 'Tool finished';
    if (normalized.contains('tool') || normalized == 'mcptoolcall') {
      final name = toolName;
      return name == null ? 'Using a tool' : 'Using ${_words(name)}';
    }
    if (type.isEmpty || type == 'record') return 'Agent activity';
    return _words(type);
  }

  String? get toolName {
    final value = payload;
    if (value is! Map) return null;
    final direct = value['name'];
    if (direct is String && direct.trim().isNotEmpty) return direct;
    final parts = value['parts'];
    if (parts is List) {
      for (final part in parts) {
        if (part is Map && part['type'] == 'tool') {
          final tool = part['tool'];
          if (tool is String && tool.trim().isNotEmpty) return tool;
        }
      }
    }
    final update =
        _mapAt(value, const ['params', 'update']) ??
        _mapAt(value, const ['update']);
    final title = update?['title'];
    return title is String && title.trim().isNotEmpty ? title : null;
  }

  String get activityDetail {
    final value = payload;
    if (value is Map) {
      final parts = value['parts'];
      if (parts is List) {
        final details = <String>[];
        for (final part in parts) {
          if (part is! Map || part['type'] != 'tool') continue;
          final name = part['tool'];
          final state = part['state'];
          final title = state is Map ? state['title'] : null;
          final output = state is Map ? state['output'] : null;
          details.add(
            [
              if (name is String) name,
              if (title is String && title != name) title,
              if (output is String && output.trim().isNotEmpty) output,
            ].join('\n'),
          );
        }
        if (details.isNotEmpty) return details.join('\n\n');
      }
      for (final key in const ['arguments', 'output', 'result', 'error']) {
        final detail = _readable(value[key]);
        if (detail != null && detail.trim().isNotEmpty) return detail;
      }
    }
    return text;
  }

  String get timestamp {
    final value = payload;
    if (value is! Map) return '';
    for (final key in const ['timestamp', 'created_at', 'time']) {
      final candidate = value[key];
      if (candidate is String && candidate.isNotEmpty) return candidate;
      if (candidate is num) {
        final milliseconds = candidate > 100000000000
            ? candidate.toInt()
            : candidate.toInt() * 1000;
        return DateTime.fromMillisecondsSinceEpoch(
          milliseconds,
        ).toLocal().toIso8601String();
      }
    }
    for (final path in const [
      ['info', 'time', 'created'],
      ['info', 'time', 'completed'],
      ['params', 'update', 'timestamp'],
      ['update', 'timestamp'],
    ]) {
      final candidate = _at(value, path);
      if (candidate is String && candidate.isNotEmpty) return candidate;
      if (candidate is num) {
        final milliseconds = candidate > 100000000000
            ? candidate.toInt()
            : candidate.toInt() * 1000;
        return DateTime.fromMillisecondsSinceEpoch(
          milliseconds,
        ).toLocal().toIso8601String();
      }
    }
    return '';
  }

  static String? _textContent(dynamic value) {
    if (value is String && value.trim().isNotEmpty) return value;
    if (value is Map) {
      final type = value['type'];
      if (type == 'text' || type == 'input_text' || type == 'output_text') {
        final text =
            value['text'] ?? value['input_text'] ?? value['output_text'];
        if (text is String && text.isNotEmpty) return text;
      }
      return null;
    }
    if (value is! List) return null;
    final parts = <String>[];
    for (final part in value) {
      if (part is String && part.isNotEmpty) {
        parts.add(part);
      } else if (part is Map) {
        final type = part['type'];
        if (type == 'text' || type == 'input_text' || type == 'output_text') {
          final text =
              part['text'] ?? part['input_text'] ?? part['output_text'];
          if (text is String && text.isNotEmpty) parts.add(text);
        }
      }
    }
    return parts.isEmpty ? null : parts.join('\n');
  }

  static dynamic _at(Map value, List<String> path) {
    dynamic current = value;
    for (final part in path) {
      if (current is! Map) return null;
      current = current[part];
    }
    return current;
  }

  static Map? _mapAt(Map value, List<String> path) {
    final result = _at(value, path);
    return result is Map ? result : null;
  }

  static String _words(String value) => value
      .replaceAllMapped(
        RegExp(r'([a-z0-9])([A-Z])'),
        (match) => '${match.group(1)} ${match.group(2)}',
      )
      .replaceAll('_', ' ')
      .trim();

  static String? _readable(dynamic value) {
    if (value is String && value.isNotEmpty) return value;
    if (value is List) {
      final parts = value.map(_readable).whereType<String>().toList();
      return parts.isEmpty ? null : parts.join('\n');
    }
    if (value is Map) {
      for (final key in const [
        'text',
        'content',
        'message',
        'output',
        'result',
        'error',
      ]) {
        final candidate = _readable(value[key]);
        if (candidate != null && candidate.isNotEmpty) return candidate;
      }
    }
    return null;
  }

  static String _pretty(dynamic value) {
    try {
      return const JsonEncoder.withIndent('  ').convert(value);
    } catch (_) {
      return '$value';
    }
  }
}

class DaemonStatus {
  const DaemonStatus({
    required this.uptimeSeconds,
    required this.subscribers,
    required this.configGeneration,
    required this.pendingRestore,
  });

  final int uptimeSeconds, subscribers, configGeneration;
  final int? pendingRestore;

  factory DaemonStatus.fromJson(Map<String, dynamic> value) => DaemonStatus(
    uptimeSeconds: (value['uptime_secs'] as num?)?.toInt() ?? 0,
    subscribers: (value['subscriber_count'] as num?)?.toInt() ?? 0,
    configGeneration: (value['config_generation'] as num?)?.toInt() ?? 0,
    pendingRestore: (value['pending_restore'] as num?)?.toInt(),
  );
}

class ClientInfo {
  const ClientInfo({
    required this.version,
    required this.commit,
    required this.configPath,
    required this.socketPath,
    required this.capabilities,
  });

  final String version, commit, configPath, socketPath;
  final List<String> capabilities;

  factory ClientInfo.fromJson(Map<String, dynamic> value) => ClientInfo(
    version: value['version'] as String? ?? 'unknown',
    commit: value['commit'] as String? ?? 'unknown',
    configPath: value['config_path'] as String? ?? '',
    socketPath: value['socket_path'] as String? ?? '',
    capabilities: (value['capabilities'] as List? ?? const []).cast<String>(),
  );
}

class GraveEntry {
  const GraveEntry({
    required this.id,
    required this.bootId,
    required this.tmuxSessionId,
    required this.destroyedAt,
    required this.cause,
    required this.detectedBy,
    required this.pane,
    required this.revivedAt,
    required this.revivedAs,
  });

  final String id, bootId, tmuxSessionId, cause, detectedBy, revivedAs;
  final int destroyedAt;
  final int? revivedAt;
  final PaneInfo pane;

  bool get revived => revivedAt != null || revivedAs.isNotEmpty;

  factory GraveEntry.fromJson(Map<String, dynamic> value) => GraveEntry(
    id: value['grave_id'] as String,
    bootId: value['tmux_boot_id'] as String? ?? '',
    tmuxSessionId: value['tmux_session_id'] as String? ?? '',
    destroyedAt: (value['destroyed_at'] as num?)?.toInt() ?? 0,
    cause: value['cause'] as String? ?? 'unknown',
    detectedBy: value['detected_by'] as String? ?? '',
    pane: PaneInfo.fromJson(value['pane'] as Map<String, dynamic>),
    revivedAt: (value['revived_at'] as num?)?.toInt(),
    revivedAs: value['revived_as'] as String? ?? '',
  );
}

String formatAge(int timestamp) {
  if (timestamp <= 0) return 'unknown';
  final difference = DateTime.now().difference(
    DateTime.fromMillisecondsSinceEpoch(timestamp * 1000),
  );
  if (difference.inSeconds < 60) return '${difference.inSeconds}s ago';
  if (difference.inMinutes < 60) return '${difference.inMinutes}m ago';
  if (difference.inHours < 24) return '${difference.inHours}h ago';
  return '${difference.inDays}d ago';
}

String formatDuration(int seconds) {
  final duration = Duration(seconds: seconds);
  if (duration.inDays > 0) {
    return '${duration.inDays}d ${duration.inHours.remainder(24)}h';
  }
  if (duration.inHours > 0) {
    return '${duration.inHours}h ${duration.inMinutes.remainder(60)}m';
  }
  if (duration.inMinutes > 0) {
    return '${duration.inMinutes}m ${duration.inSeconds.remainder(60)}s';
  }
  return '${duration.inSeconds}s';
}
