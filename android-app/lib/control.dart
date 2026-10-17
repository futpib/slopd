import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:dartssh2/dartssh2.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';

import 'transport.dart';
import 'identities.dart';

class Profile {
  Profile({
    required this.name,
    required this.host,
    required this.user,
    this.port = 22,
    this.iroh = false,
    this.privateKey = '',
    this.password = '',
    this.passphrase = '',
    this.keyId = '',
    this.additionalKeyIds = const [],
    this.relays = const [],
    this.executable = 'slopctl',
    this.socket = '',
    this.config = '',
  });
  final String name,
      host,
      user,
      privateKey,
      password,
      passphrase,
      keyId,
      executable,
      socket,
      config;
  final int port;
  final bool iroh;
  final List<String> relays;
  final List<String> additionalKeyIds;
  List<String> get selectedKeyIds => [
    if (keyId.isNotEmpty) keyId,
    ...additionalKeyIds,
  ];
  String get hostIdentity =>
      '${iroh ? 'iroh' : 'ssh'}:$host:${iroh ? 22 : port}';
  Map<String, dynamic> toJson() => {
    'name': name,
    'host': host,
    'user': user,
    'port': port,
    'iroh': iroh,
    'privateKey': privateKey,
    'password': password,
    'passphrase': passphrase,
    'keyId': keyId,
    'additionalKeyIds': additionalKeyIds,
    'relays': relays,
    'executable': executable,
    'socket': socket,
    'config': config,
  };
  factory Profile.fromJson(Map<String, dynamic> value) => Profile(
    name: value['name'] as String,
    host: value['host'] as String,
    user: value['user'] as String,
    port: value['port'] as int,
    iroh: value['iroh'] as bool,
    privateKey: value['privateKey'] as String,
    password: value['password'] as String,
    passphrase: value['passphrase'] as String,
    keyId: value['keyId'] as String? ?? '',
    additionalKeyIds:
        (value['additionalKeyIds'] as List?)?.cast<String>() ?? const [],
    executable: value['executable'] as String? ?? 'slopctl',
    socket: value['socket'] as String? ?? '',
    config: value['config'] as String? ?? '',
    relays: (value['relays'] as List?)?.cast<String>() ?? const [],
  );
}

class ProfileStore {
  const ProfileStore();
  static const storage = FlutterSecureStorage();
  Future<List<Profile>> load() async {
    final value = await storage.read(key: 'profiles');
    final profiles = value == null
        ? <Profile>[]
        : (jsonDecode(value) as List)
              .map((v) => Profile.fromJson(v as Map<String, dynamic>))
              .toList();
    var migrated = false;
    for (var i = 0; i < profiles.length; i++) {
      final profile = profiles[i];
      if (profile.privateKey.isEmpty) continue;
      Identity identity;
      try {
        identity = await IdentityStore().import(
          '${profile.name} key',
          profile.privateKey,
          profile.passphrase,
        );
      } catch (_) {
        // Preserve legacy credentials if they cannot be migrated; the connection remains editable.
        continue;
      }
      profiles[i] = Profile.fromJson({
        ...profile.toJson(),
        'keyId': identity.id,
        'privateKey': '',
        'passphrase': '',
      });
      migrated = true;
    }
    if (migrated) await save(profiles);
    return profiles;
  }

  Future<void> save(List<Profile> profiles) => storage.write(
    key: 'profiles',
    value: jsonEncode(profiles.map((p) => p.toJson()).toList()),
  );
  Future<String?> hostKey(String identity) =>
      storage.read(key: 'host:$identity');
  Future<void> trust(String identity, String fingerprint) =>
      storage.write(key: 'host:$identity', value: fingerprint);
  Future<void> forget(String identity) => storage.delete(key: 'host:$identity');
}

String shellQuote(String value) => "'${value.replaceAll("'", "'\"'\"'")}'";

abstract class ControlApi {
  Future<CommandResult> command(
    List<String> arguments, {
    Duration timeout = const Duration(seconds: 45),
  });
  Future<CommandStream> openStream(List<String> arguments) => Future.error(
    UnsupportedError('This control transport does not support streams'),
  );
  Future<void> close();
}

class CommandResult {
  const CommandResult(this.stdout, this.stderr, this.exitCode);
  final String stdout, stderr;
  final int exitCode;
}

class CommandStream {
  const CommandStream({
    required this.lines,
    required this.errors,
    required this.done,
    required this.close,
  });

  final Stream<String> lines, errors;
  final Future<int> done;
  final void Function() close;
}

class ConnectionLost implements Exception {
  const ConnectionLost();
  @override
  String toString() => 'Connection lost. Reconnect to continue.';
}

/// Keeps the dashboard and its drafts alive while replacing the transport.
/// Requests are never retried: a lost response may hide a completed mutation.
class ControlConnection extends ChangeNotifier implements ControlApi {
  ControlConnection(this._api, this._connect);
  ControlApi? _api;
  final Future<ControlApi> Function() _connect;
  Future<void>? _reconnection;
  bool disconnected = false, _closed = false;
  bool get reconnecting => _reconnection != null;

  @override
  Future<CommandResult> command(
    List<String> arguments, {
    Duration timeout = const Duration(seconds: 45),
  }) async {
    final api = _api;
    if (_closed || disconnected || reconnecting || api == null) {
      throw const ConnectionLost();
    }
    try {
      final result = await api.command(arguments, timeout: timeout);
      if (_closed || api != _api) throw const ConnectionLost();
      return result;
    } on ConnectionLost {
      if (!_closed && api == _api && !disconnected) {
        disconnected = true;
        notifyListeners();
      }
      rethrow;
    }
  }

  Future<void> reconnect() {
    if (_closed) return Future.error(const ConnectionLost());
    if (_reconnection != null) return _reconnection!;
    final done = Completer<void>();
    _reconnection = done.future;
    disconnected = true;
    notifyListeners();
    () async {
      try {
        final old = _api;
        _api = null;
        await old?.close();
        if (_closed) throw const ConnectionLost();
        final next = await _connect();
        if (_closed) {
          await next.close();
          throw const ConnectionLost();
        }
        _api = next;
        disconnected = false;
        done.complete();
      } catch (error, stack) {
        done.completeError(error, stack);
      } finally {
        _reconnection = null;
        if (!_closed) notifyListeners();
      }
    }();
    return done.future;
  }

  @override
  Future<CommandStream> openStream(List<String> arguments) async {
    final api = _api;
    if (_closed || disconnected || reconnecting || api == null) {
      throw const ConnectionLost();
    }
    try {
      final stream = await api.openStream(arguments);
      if (_closed || api != _api) {
        stream.close();
        throw const ConnectionLost();
      }
      return CommandStream(
        lines: stream.lines,
        errors: stream.errors,
        done: stream.done.catchError((Object error, StackTrace stack) {
          if (error is ConnectionLost &&
              !_closed &&
              api == _api &&
              !disconnected) {
            disconnected = true;
            notifyListeners();
          }
          Error.throwWithStackTrace(error, stack);
        }),
        close: stream.close,
      );
    } on ConnectionLost {
      if (!_closed && api == _api && !disconnected) {
        disconnected = true;
        notifyListeners();
      }
      rethrow;
    }
  }

  @override
  Future<void> close() async {
    if (_closed) return;
    _closed = true;
    final api = _api;
    _api = null;
    await api?.close();
  }
}

typedef AuthenticationRequest =
    Future<List<String>?> Function(SSHUserInfoRequest request, bool retry);

class PasswordChangeRequest extends SSHUserInfoRequest {
  PasswordChangeRequest(String instruction)
    : super('Change SSH password', instruction, [
        SSHUserInfoPrompt('Current password', false),
        SSHUserInfoPrompt('New password', false),
        SSHUserInfoPrompt('Confirm new password', false),
      ]);
}

class AuthenticationCancellation {
  final _cancelled = Completer<void>();
  bool get isCancelled => _cancelled.isCompleted;
  Future<void> get whenCancelled => _cancelled.future;
  void cancel() {
    if (!isCancelled) _cancelled.complete();
  }
}

class AuthenticationCancelled implements Exception {
  const AuthenticationCancelled();
  @override
  String toString() => 'Authentication cancelled';
}

class SshControl implements ControlApi {
  SshControl._(this._ssh, this.profile, this._tunnel);
  final SSHClient _ssh;
  final Profile profile;
  final int? _tunnel;
  bool _closed = false;

  static Future<SshControl> connect(
    Profile profile,
    Future<bool> Function(String type, String fingerprint) verify, {
    AuthenticationRequest? onAuthenticationRequest,
    AuthenticationCancellation? cancellation,
  }) async {
    for (var attempt = 0; ; attempt++) {
      var submitted = false;
      try {
        return await _connect(
          profile,
          verify,
          onAuthenticationRequest: onAuthenticationRequest,
          retry: attempt > 0,
          onSubmitted: () => submitted = true,
          cancellation: cancellation,
        );
      } on SSHAuthFailError {
        // dartssh2 tries each authentication method once per connection.
        if (onAuthenticationRequest == null || !submitted || attempt >= 2) {
          rethrow;
        }
      }
    }
  }

  static Future<SshControl> _connect(
    Profile profile,
    Future<bool> Function(String type, String fingerprint) verify, {
    required AuthenticationRequest? onAuthenticationRequest,
    required bool retry,
    required void Function() onSubmitted,
    AuthenticationCancellation? cancellation,
  }) async {
    int? tunnel;
    SSHClient? client;
    SSHSocket? socket;
    (Object, StackTrace)? verificationFailure;
    var cancelled = false, active = true, savedPasswordUsed = retry;
    Future<List<String>?> ask(SSHUserInfoRequest request) async {
      if (!active) return null;
      if (request.prompts.isEmpty && onAuthenticationRequest == null) return [];
      final responses = await onAuthenticationRequest!(request, retry);
      if (!active) return null;
      if (responses == null) {
        cancelled = true;
        client?.close();
        return null;
      }
      if (request.prompts.isNotEmpty) onSubmitted();
      return responses;
    }

    try {
      final selected = <SSHIdentity>[];
      for (final id in profile.selectedKeyIds) {
        selected.addAll((await IdentityStore().get(id)).createIdentities());
      }
      if (selected.isEmpty && profile.privateKey.trim().isNotEmpty) {
        selected.addAll(
          SSHKeyPair.fromPem(
            profile.privateKey,
            profile.passphrase.isEmpty ? null : profile.passphrase,
          ),
        );
      }
      final identities = selected.isEmpty ? null : selected;
      if (profile.iroh) {
        tunnel = await IrohTransport.connect(profile.host, profile.relays);
      }
      socket = await SSHSocket.connect(
        tunnel == null ? profile.host : '127.0.0.1',
        tunnel ?? profile.port,
        timeout: const Duration(seconds: 30),
      );
      client = SSHClient(
        socket,
        username: profile.user,
        identities: identities,
        onPasswordRequest: () async {
          if (!active) return null;
          if (!savedPasswordUsed && profile.password.isNotEmpty) {
            savedPasswordUsed = true;
            onSubmitted();
            return profile.password;
          }
          if (onAuthenticationRequest == null) return null;
          final responses = await ask(
            SSHUserInfoRequest('SSH password', '', [
              SSHUserInfoPrompt('Password', false),
            ]),
          );
          return responses?.single;
        },
        onUserInfoRequest: onAuthenticationRequest == null ? null : ask,
        onChangePasswordRequest: (prompt) async {
          if (onAuthenticationRequest == null) return null;
          final responses = await ask(PasswordChangeRequest(prompt));
          if (responses == null) return null;
          return SSHChangePasswordResponse(responses[0], responses[1]);
        },
        onVerifyHostKey: (type, fingerprint) async {
          try {
            return await verify(type, utf8.decode(fingerprint));
          } catch (error, stack) {
            verificationFailure = (error, stack);
            return false;
          }
        },
        keepAliveInterval: const Duration(seconds: 15),
      );
      await Future.any([
        client.authenticated,
        if (cancellation != null)
          cancellation.whenCancelled.then((_) {
            throw const AuthenticationCancelled();
          }),
      ]).timeout(const Duration(minutes: 2));
      return SshControl._(client, profile, tunnel);
    } catch (_) {
      client?.close();
      if (client == null) socket?.close();
      if (tunnel != null) await IrohTransport.disconnect(tunnel);
      if (cancelled) throw const AuthenticationCancelled();
      final failure = verificationFailure;
      if (failure != null) Error.throwWithStackTrace(failure.$1, failure.$2);
      rethrow;
    } finally {
      active = false;
    }
  }

  String commandLine(List<String> arguments) => [
    shellQuote(profile.executable),
    if (profile.config.isNotEmpty) ...['--config', shellQuote(profile.config)],
    if (profile.socket.isNotEmpty) ...['--socket', shellQuote(profile.socket)],
    ...arguments.map(shellQuote),
  ].join(' ');

  @override
  Future<CommandResult> command(
    List<String> arguments, {
    Duration timeout = const Duration(seconds: 45),
  }) async {
    try {
      return await _command(arguments, timeout);
    } on TimeoutException {
      _ssh.close();
      throw const ConnectionLost();
    } catch (_) {
      if (_ssh.isClosed) throw const ConnectionLost();
      rethrow;
    }
  }

  Future<CommandResult> _command(
    List<String> arguments,
    Duration timeout,
  ) async {
    final session = await _ssh
        .execute(commandLine(arguments))
        .timeout(const Duration(seconds: 15));
    Future<List<int>> bounded(Stream<Uint8List> stream) async {
      final builder = BytesBuilder(copy: false);
      await for (final bytes in stream) {
        builder.add(bytes);
        if (builder.length > 4 * 1024 * 1024) {
          session.close();
          throw StateError('slopctl response exceeded 4 MiB');
        }
      }
      return builder.takeBytes();
    }

    try {
      final output = Future.wait([
        bounded(session.stdout),
        bounded(session.stderr),
      ]);
      final streams = await output.timeout(timeout);
      await session.done.timeout(const Duration(seconds: 5));
      final result = CommandResult(
        utf8.decode(streams[0]),
        utf8.decode(streams[1], allowMalformed: true),
        session.exitCode ?? 255,
      );
      if (result.exitCode != 0) {
        final detail = result.stderr.trim().isNotEmpty
            ? result.stderr.trim()
            : result.stdout.trim();
        throw StateError(
          detail.isEmpty
              ? 'slopctl exited ${result.exitCode}'
              : 'slopctl exited ${result.exitCode}: $detail',
        );
      }
      return result;
    } finally {
      session.close();
    }
  }

  @override
  Future<CommandStream> openStream(List<String> arguments) async {
    if (_closed || _ssh.isClosed) throw const ConnectionLost();
    late final SSHSession session;
    try {
      session = await _ssh
          .execute(commandLine(arguments))
          .timeout(const Duration(seconds: 15));
    } on TimeoutException {
      _ssh.close();
      throw const ConnectionLost();
    } catch (_) {
      if (_ssh.isClosed) throw const ConnectionLost();
      rethrow;
    }

    final lines = StreamController<String>();
    final errors = StreamController<String>();
    final stderr = StringBuffer();
    var cancelled = false;

    Future<void> pump(
      Stream<Uint8List> input,
      StreamController<String> output, {
      StringBuffer? copy,
    }) async {
      await for (final line
          in input
              .cast<List<int>>()
              .transform(utf8.decoder)
              .transform(const LineSplitter())) {
        copy?.writeln(line);
        if (!output.isClosed) output.add(line);
      }
    }

    final done = () async {
      try {
        await Future.wait([
          pump(session.stdout, lines),
          pump(session.stderr, errors, copy: stderr),
          session.done,
        ]);
        if (cancelled) return 0;
        if (_ssh.isClosed) throw const ConnectionLost();
        final exitCode = session.exitCode ?? 255;
        if (exitCode != 0) {
          final detail = stderr.toString().trim();
          throw StateError(
            detail.isEmpty
                ? 'slopctl exited $exitCode'
                : 'slopctl exited $exitCode: $detail',
          );
        }
        return exitCode;
      } catch (_) {
        if (_ssh.isClosed) throw const ConnectionLost();
        rethrow;
      } finally {
        await lines.close();
        await errors.close();
        session.close();
      }
    }();

    return CommandStream(
      lines: lines.stream,
      errors: errors.stream,
      done: done,
      close: () {
        cancelled = true;
        session.close();
      },
    );
  }

  @override
  Future<void> close() async {
    if (_closed) return;
    _closed = true;
    _ssh.close();
    if (_tunnel != null) await IrohTransport.disconnect(_tunnel);
  }
}
