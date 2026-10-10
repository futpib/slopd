import 'dart:async';
import 'dart:typed_data';

import 'package:dartssh2/src/sftp/sftp_client.dart';
import 'package:dartssh2/src/utils/stream.dart';

/// The default amount of data to send in a single SFTP packet.
///
/// From the SFTP spec it's safe to send up to 32KB of data in a single packet.
/// To strike a balance between capability and performance, we choose 16KB.
const defaultChunkSize = 16 * 1024;

/// The amount of data historically sent in a single SFTP packet.
@Deprecated('Use defaultChunkSize instead. Will be removed in 5.0.0.')
const chunkSize = defaultChunkSize;

/// The default maximum number of unacknowledged SFTP write requests.
///
/// This matches OpenSSH's `DEFAULT_NUM_REQUESTS`.
const defaultMaxPendingRequests = 64;

/// The byte equivalent of the default request window.
///
/// Nothing reads this any more. Uploads are bounded by a count of outstanding
/// requests rather than by a byte window, so this describes a mechanism that no
/// longer exists and is kept only so existing references still compile.
/// Configure [defaultMaxPendingRequests] through [SftpFile.write] or
/// [SftpFile.writeBytes] instead.
@Deprecated(
  'Uploads are bounded by outstanding request count, not by a byte window. '
  'Use defaultMaxPendingRequests instead. Will be removed in 5.0.0.',
)
const maxBytesOnTheWire = defaultChunkSize * defaultMaxPendingRequests;

/// Holds the state of a streaming write operation from [stream] to [file].
class SftpFileWriter with DoneFuture {
  /// The remote file to write to.
  final SftpFile file;

  /// The stream of data to write to [file].
  final Stream<Uint8List> stream;

  /// The offset in [file] to start writing to.
  final int offset;

  /// Called when [bytes] of data have been successfully written to [file].
  final Function(int bytes)? onProgress;

  /// Maximum size of each SFTP write request.
  final int _chunkSize;

  /// Maximum number of write requests waiting for acknowledgement.
  final int _maxPendingRequests;

  /// Creates a new [SftpFileWriter]. The upload process is started immediately
  /// after construction.
  SftpFileWriter(
    this.file,
    this.stream,
    this.offset,
    this.onProgress, {
    int chunkSize = defaultChunkSize,
    int maxPendingRequests = defaultMaxPendingRequests,
  })  : _chunkSize = chunkSize,
        _maxPendingRequests = maxPendingRequests {
    if (offset < 0) {
      throw ArgumentError.value(offset, 'offset', 'must not be negative');
    }
    if (_chunkSize <= 0) {
      throw ArgumentError.value(_chunkSize, 'chunkSize', 'must be positive');
    }
    if (_maxPendingRequests <= 0) {
      throw ArgumentError.value(
        _maxPendingRequests,
        'maxPendingRequests',
        'must be positive',
      );
    }

    _subscription = stream.transform(MaxChunkSize(_chunkSize)).listen(
          _handleLocalData,
          onError: _handleSourceError,
          onDone: _handleLocalDone,
        );
  }

  /// The subscription for [stream]. We use this to pause and resume the data
  /// source.
  late final StreamSubscription<Uint8List> _subscription;

  final _doneCompleter = Completer<void>();

  /// Bytes assigned an offset and submitted for writing.
  var _bytesScheduled = 0;

  /// Bytes of data that have been acknowledged by the remote host.
  var _bytesAcked = 0;

  /// Number of write requests sent but not yet acknowledged.
  var _pendingWrites = 0;

  /// Whether [stream] has emitted all of its data.
  var _streamDone = false;

  var _userPaused = false;
  var _subscriptionPaused = false;
  var _stopped = false;
  var _aborted = false;
  Object? _error;
  StackTrace? _errorStackTrace;
  Future<void>? _cancelFuture;

  /// A [Future] that completes when:
  ///
  /// - All data from [stream] has been written to [file]
  /// - Or the write operation has been aborted by calling [abort].
  @override
  Future<void> get done => _doneCompleter.future;

  /// The number of bytes that have been successfully written to [file].
  int get progress => _bytesAcked;

  /// Stops [stream] from emitting more data. Returns a [Future] that completes
  /// when the underlying data source of [stream] has been successfully closed.
  ///
  /// Calling [abort] will make [done] to complete immediately.
  Future<void> abort() async {
    if (_doneCompleter.isCompleted) return;
    _aborted = true;
    _stopped = true;
    _doneCompleter.complete();
    await _subscription.cancel();
  }

  /// Pauses [stream] from emitting more data. It's safe to call this even if
  /// the stream is already paused. Use [resume] to resume the operation.
  void pause() {
    _userPaused = true;
    _syncSubscriptionState();
  }

  /// Resumes [stream] after it has been paused. It's safe to call this even if
  /// the stream is not paused. Use [pause] to pause the operation.
  void resume() {
    _userPaused = false;
    _syncSubscriptionState();
  }

  /// Handles the incoming data chunks from the stream.
  ///
  /// At most [_maxPendingRequests] invocations remain in flight. Errors are
  /// observed here, so they complete [done] instead of escaping an async stream
  /// callback as an unhandled error.
  void _handleLocalData(Uint8List chunk) {
    if (_stopped) return;

    final chunkWriteOffset = offset + _bytesScheduled;
    _bytesScheduled += chunk.length;
    _pendingWrites++;
    _syncSubscriptionState();

    file
        .writeBytes(
      chunk,
      offset: chunkWriteOffset,
      chunkSize: chunk.length,
      maxPendingRequests: 1,
    )
        .then<void>(
      (_) {
        if (_aborted) return;
        try {
          _bytesAcked += chunk.length;
          onProgress?.call(_bytesAcked);
        } catch (error, stackTrace) {
          _stopWithError(error, stackTrace);
        }
      },
      onError: (Object error, StackTrace stackTrace) {
        _stopWithError(error, stackTrace);
      },
    ).whenComplete(() {
      _pendingWrites--;
      _syncSubscriptionState();
      _tryComplete();
    });
  }

  void _handleSourceError(Object error, StackTrace stackTrace) {
    _stopWithError(error, stackTrace);
  }

  /// Handles the completion of the data stream.
  ///
  /// This function is triggered when the stream has finished emitting all its
  /// data. It checks if all data has been successfully acknowledged and
  /// marks the operation as complete by calling `_doneCompleter.complete()`
  /// if no more data remains to be processed.
  void _handleLocalDone() {
    _streamDone = true;
    _tryComplete();
  }

  void _stopWithError(Object error, StackTrace stackTrace) {
    _error ??= error;
    _errorStackTrace ??= stackTrace;
    _stopped = true;
    _syncSubscriptionState();

    final cancelFuture = _cancelFuture ??= _subscription.cancel();
    cancelFuture.then<void>(
      (_) {
        _streamDone = true;
        _tryComplete();
      },
      onError: (Object cancelError, StackTrace cancelStackTrace) {
        _error ??= cancelError;
        _errorStackTrace ??= cancelStackTrace;
        _streamDone = true;
        _tryComplete();
      },
    );
  }

  void _syncSubscriptionState() {
    if (_aborted) return;
    final shouldPause =
        _userPaused || _stopped || _pendingWrites >= _maxPendingRequests;
    if (shouldPause == _subscriptionPaused) return;

    _subscriptionPaused = shouldPause;
    if (shouldPause) {
      _subscription.pause();
    } else {
      _subscription.resume();
    }
  }

  void _tryComplete() {
    if (_doneCompleter.isCompleted || !_streamDone || _pendingWrites != 0) {
      return;
    }

    final error = _error;
    if (error != null) {
      _doneCompleter.completeError(error, _errorStackTrace!);
    } else {
      _doneCompleter.complete();
    }
  }
}

/// Implements [Future] interface for [SftpFileWriter].
///
/// This is for compatibility with earlier versions of dartssh2 and dartssh2.
mixin DoneFuture implements Future {
  Future<void> get done;

  @override
  Stream<void> asStream() => done.asStream();

  @override
  Future<void> catchError(
    Function onError, {
    bool Function(Object error)? test,
  }) =>
      done.catchError(onError, test: test);

  @override
  Future<S> then<S>(FutureOr<S> Function(void) onValue, {Function? onError}) =>
      done.then(onValue, onError: onError);

  @override
  Future<void> whenComplete(FutureOr Function() action) =>
      done.whenComplete(action);

  @override
  Future<void> timeout(
    Duration timeLimit, {
    FutureOr<void> Function()? onTimeout,
  }) =>
      done.timeout(timeLimit, onTimeout: onTimeout);
}
