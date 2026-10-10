import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:dartssh2/src/http/http_date.dart';
import 'package:dartssh2/src/http/http_exception.dart';
import 'package:dartssh2/src/http/line_decoder.dart';
import 'package:dartssh2/src/http/http_content_type.dart';
import 'package:dartssh2/src/http/http_headers.dart';
import 'package:dartssh2/src/socket/ssh_socket.dart';
import 'package:dartssh2/src/ssh_client.dart';

/// A HTTP client that works over SSH port forwarding.
///
/// This is a very basic implementation of a HTTP client, has the following
/// limitations:
///
/// * Only supports HTTP/1.0 and HTTP/1.1
/// * Only supports "identity" encoding
/// * No keep-alive
/// * No automatic redirects
/// * No support for https
///
/// This client intends to be used for communicating with services on the ssh
/// host that are easier to communicate with using HTTP. *Not* for communicating
/// with API endpoints on the internet.
class SSHHttpClient {
  const SSHHttpClient(this.client, {this.idleTimeout});

  final SSHClient client;

  /// Maximum time to wait between two pieces of a response before giving up.
  ///
  /// A response that carries neither `Content-Length` nor
  /// `Transfer-Encoding: chunked` is delimited by the peer closing the
  /// connection (RFC 9112 §6.3), so it has to be read until end of stream.
  /// A peer that sends such a response and then holds the connection open
  /// would keep that read waiting forever, and nothing else here bounds it.
  ///
  /// When set, a [TimeoutException] is thrown if the server goes this long
  /// without sending anything further. It is an inactivity timeout, not a
  /// deadline for the whole response, so a large but steadily arriving body
  /// is never cut short. Null (the default) keeps the previous unbounded
  /// behaviour.
  final Duration? idleTimeout;

  /// Send a HTTP request to [uri] with the provided [method].
  SSHHttpClientRequest request(String method, Uri uri, bool body) {
    final request =
        SSHHttpClientRequest._(client, method, uri, body, idleTimeout);
    return request;
  }

  /// Send a GET request to the provided URL.
  SSHHttpClientRequest get(Uri uri) => request('GET', uri, false);

  /// Send a POST request to the provided URL.
  SSHHttpClientRequest post(Uri uri) => request('POST', uri, true);

  /// Send a DELETE request to the provided URL.
  SSHHttpClientRequest delete(Uri uri) => request('DELETE', uri, false);

  /// Send a PUT request to the provided URL.
  SSHHttpClientRequest put(Uri uri) => request('PUT', uri, true);
}

/// Version of HTTP to use for the request.
enum SSHHttpProtocolVersion {
  http10('1.0'),

  http11('1.1'),
  ;

  const SSHHttpProtocolVersion(this.name);

  final String name;
}

/// HTTP request created on a [SSHHttpClient].
class SSHHttpClientRequest {
  /// The underlying SSH client.
  final SSHClient client;

  /// The headers associated with the HTTP request.
  late final SSHHttpHeaders headers = _SSHHttpClientRequestHeaders(this);

  /// The type of HTTP request being made.
  final String method;

  /// The Uri the HTTP request will be sent to.
  final Uri uri;

  /// The default encoding for the HTTP request (UTF8).
  final Encoding encoding = utf8;

  /// The body of the HTTP request. This can be empty if there is no body
  /// associated with the request.
  final BytesBuilder? _body;

  /// The HTTP protocol version used for the request. Defaults to HTTP/1.1.
  ///
  /// Set this to [SSHHttpProtocolVersion.http10] to avoid the server from
  /// sending a `Transfer-Encoding: chunked` response.
  var protocolVersion = SSHHttpProtocolVersion.http11;

  /// The length of the request body. Is set to `-1` when no body exists.
  int get contentLength => hasBody ? _body!.length : -1;

  /// Whether or not the HTTP request has a body.
  bool get hasBody => _body != null;

  /// See [SSHHttpClient.idleTimeout].
  final Duration? idleTimeout;

  SSHHttpClientRequest._(
    this.client,
    this.method,
    this.uri,
    bool body,
    this.idleTimeout,
  ) : _body = body ? BytesBuilder() : null;

  /// Write content into the body of the HTTP request.
  void write(Object? obj) {
    if (hasBody) {
      if (obj != null) {
        _body!.add(encoding.encoder.convert(obj.toString()));
      }
    } else {
      throw StateError('write not allowed for method $method');
    }
  }

  /// Send the HTTP request and get the response.
  Future<SSHHttpClientResponse> close() async {
    var queryString = '';
    if (uri.hasQuery) {
      final query = StringBuffer();
      query.write('?');
      uri.queryParameters.forEach((k, v) {
        query.write(Uri.encodeComponent(k));
        query.write('=');
        query.write(Uri.encodeComponent(v));
        query.write('&');
      });
      queryString = query.toString().substring(0, query.length - 1);
    }
    final buffer = StringBuffer();

    final path = uri.path.isEmpty ? '/' : uri.path;

    buffer.write('$method $path$queryString HTTP/${protocolVersion.name}\r\n');

    headers.forEach((name, values) {
      for (var value in values) {
        buffer.write('$name: $value\r\n');
      }
    });

    buffer.write('\r\n');

    if (hasBody) {
      buffer.write(String.fromCharCodes(_body!.takeBytes()));
    }

    final socket = await client.forwardLocal(uri.host, uri.port);
    socket.sink.add(buffer.toString().codeUnits);
    return SSHHttpClientResponse.from(
      socket,
      method: method,
      idleTimeout: idleTimeout,
    );
  }
}

class _SSHHttpClientRequestHeaders implements SSHHttpHeaders {
  final Map<String, List<String>> _headers = <String, List<String>>{};

  final SSHHttpClientRequest _request;

  @override
  SSHContentType? contentType;

  _SSHHttpClientRequestHeaders(this._request);

  @override
  List<String>? operator [](String name) {
    switch (name) {
      case SSHHttpHeaders.acceptCharsetHeader:
        return ['utf-8'];
      case SSHHttpHeaders.acceptEncodingHeader:
        return ['identity'];
      case SSHHttpHeaders.connectionHeader:
        return ['close'];
      case SSHHttpHeaders.contentLengthHeader:
        if (!_request.hasBody) {
          return null;
        }
        return [contentLength.toString()];
      case SSHHttpHeaders.contentTypeHeader:
        if (contentType == null) {
          return null;
        }
        return [contentType.toString()];
      case SSHHttpHeaders.hostHeader:
        return ['$host:$port'];
      default:
        final values = _headers[name];
        if (values == null || values.isEmpty) {
          return null;
        }
        return values.map<String>((e) => e.toString()).toList(growable: false);
    }
  }

  /// Add [value] to the list of values associated with header [name].
  @override
  void add(String name, Object value, {bool preserveHeaderCase = false}) {
    switch (name) {
      case SSHHttpHeaders.acceptCharsetHeader:
      case SSHHttpHeaders.acceptEncodingHeader:
      case SSHHttpHeaders.connectionHeader:
      case SSHHttpHeaders.contentLengthHeader:
      case SSHHttpHeaders.dateHeader:
      case SSHHttpHeaders.expiresHeader:
      case SSHHttpHeaders.ifModifiedSinceHeader:
      case SSHHttpHeaders.hostHeader:
        throw UnsupportedError('Unsupported or immutable property: $name');
      case SSHHttpHeaders.contentTypeHeader:
        contentType = value as SSHContentType?;
        break;
      default:
        if (_headers[name] == null) {
          _headers[name] = <String>[];
        }
        _headers[name]!.add(value as String);
    }
  }

  /// Remove [value] from the list associated with header [name].
  @override
  void remove(String name, Object value) {
    switch (name) {
      case SSHHttpHeaders.acceptCharsetHeader:
      case SSHHttpHeaders.acceptEncodingHeader:
      case SSHHttpHeaders.connectionHeader:
      case SSHHttpHeaders.contentLengthHeader:
      case SSHHttpHeaders.dateHeader:
      case SSHHttpHeaders.expiresHeader:
      case SSHHttpHeaders.ifModifiedSinceHeader:
      case SSHHttpHeaders.hostHeader:
        throw UnsupportedError('Unsupported or immutable property: $name');
      case SSHHttpHeaders.contentTypeHeader:
        if (contentType == value) {
          contentType = null;
        }
        break;
      default:
        if (_headers[name] != null) {
          _headers[name]!.remove(value);
          if (_headers[name]!.isEmpty) {
            _headers.remove(name);
          }
        }
    }
  }

  /// Remove all headers associated with key [name].
  @override
  void removeAll(String name) {
    switch (name) {
      case SSHHttpHeaders.acceptCharsetHeader:
      case SSHHttpHeaders.acceptEncodingHeader:
      case SSHHttpHeaders.connectionHeader:
      case SSHHttpHeaders.contentLengthHeader:
      case SSHHttpHeaders.dateHeader:
      case SSHHttpHeaders.expiresHeader:
      case SSHHttpHeaders.ifModifiedSinceHeader:
      case SSHHttpHeaders.hostHeader:
        throw UnsupportedError('Unsupported or immutable property: $name');
      case SSHHttpHeaders.contentTypeHeader:
        contentType = null;
        break;
      default:
        _headers.remove(name);
    }
  }

  /// Replace values associated with key [name] with [value].
  @override
  void set(String name, Object value, {bool preserveHeaderCase = false}) {
    removeAll(name);
    add(name, value, preserveHeaderCase: preserveHeaderCase);
  }

  /// Returns the values associated with key [name], if it exists, otherwise
  /// returns null.
  @override
  String? value(String name) {
    final val = this[name];
    if (val == null || val.isEmpty) {
      return null;
    } else if (val.length == 1) {
      return val[0];
    } else {
      throw SSHHttpException('header $name has more than one value');
    }
  }

  /// Iterates over all header key-value pairs and applies [f].
  @override
  void forEach(void Function(String name, List<String> values) f) {
    void forEachFunc(String name) {
      final values = this[name];
      if (values != null && values.isNotEmpty) {
        f(name, values);
      }
    }

    [
      SSHHttpHeaders.acceptCharsetHeader,
      SSHHttpHeaders.acceptEncodingHeader,
      SSHHttpHeaders.connectionHeader,
      SSHHttpHeaders.contentLengthHeader,
      SSHHttpHeaders.contentTypeHeader,
      SSHHttpHeaders.hostHeader,
    ].forEach(forEachFunc);
    _headers.keys.forEach(forEachFunc);
  }

  @override
  bool get chunkedTransferEncoding =>
      value(SSHHttpHeaders.transferEncodingHeader)?.toLowerCase() == 'chunked';

  @override
  set chunkedTransferEncoding(bool chunkedTransferEncoding) {
    throw UnsupportedError('chunked transfer is unsupported');
  }

  @override
  int get contentLength => _request.contentLength;

  @override
  set contentLength(int contentLength) {
    throw UnsupportedError('content length is automatically set');
  }

  @override
  set date(DateTime? date) {
    throw UnsupportedError('date is unsupported');
  }

  @override
  DateTime? get date => null;

  @override
  set expires(DateTime? expires) {
    throw UnsupportedError('expires is unsupported');
  }

  @override
  DateTime? get expires => null;

  @override
  set host(String? host) {
    throw UnsupportedError('host is automatically set');
  }

  @override
  String get host => _request.uri.host;

  @override
  DateTime? get ifModifiedSince => null;

  @override
  set ifModifiedSince(DateTime? ifModifiedSince) {
    throw UnsupportedError('if modified since is unsupported');
  }

  @override
  void noFolding(String name) {
    throw UnsupportedError('no folding is unsupported');
  }

  @override
  bool get persistentConnection => false;

  @override
  set persistentConnection(bool persistentConnection) {
    throw UnsupportedError('persistence connections are unsupported');
  }

  @override
  set port(int? port) {
    throw UnsupportedError('port is automatically set');
  }

  @override
  int get port => _request.uri.port;

  /// Clear all header key-value pairs.
  @override
  void clear() {
    contentType = null;
    _headers.clear();
  }
}

/// HTTP response for a [SSHHttpClientRequest].
class SSHHttpClientResponse {
  /// The headers associated with the HTTP response.
  final SSHHttpHeaders headers;

  /// A short textual description of the status code associated with the HTTP
  /// response.
  final String? reasonPhrase;

  /// The resulting HTTP status code associated with the HTTP response.
  final int? statusCode;

  /// The body of the HTTP response.
  final String? body;

  /// The length of the body associated with the HTTP response.
  int get contentLength => headers.contentLength;

  SSHHttpClientResponse._(
    Map<String, List<String>> headers, {
    this.reasonPhrase,
    this.statusCode,
    this.body,
  }) : headers = _SSHHttpClientResponseHeaders(headers);

  /// Creates an instance of [SSHHttpClientResponse] that contains the response
  /// sent by the HTTP server over [socket].
  ///
  /// [method] is the request method this is a response to. It is only used to
  /// recognise a response to `HEAD`, which never carries a body however the
  /// framing headers are set. Leaving it null assumes the response may have
  /// one.
  ///
  /// [idleTimeout] bounds the wait between two pieces of the response; see
  /// [SSHHttpClient.idleTimeout].
  static Future<SSHHttpClientResponse> from(
    SSHSocket socket, {
    String? method,
    Duration? idleTimeout,
  }) async {
    int? statusCode;
    String? reasonPhrase;
    final body = StringBuffer();
    final headers = <String, List<String>>{};

    var inHeader = false;
    var inBody = false;
    // -1 means "unknown" (no Content-Length header was present). In that
    // case the body length is not known in advance, so reading continues
    // until the stream ends (the connection is closed by the peer) rather
    // than breaking out early. Only a non-negative value here means a
    // Content-Length header was actually parsed from the response.
    var contentLength = -1;
    var contentRead = 0;
    var finished = false;

    // Chunked transfer encoding state (RFC 7230 §4.1).
    var chunked = false;
    var expectingChunkSize = false;
    var expectingChunkData = false;
    var expectingChunkCRLF = false;
    var inTrailers = false;
    var currentChunkSize = 0;

    void processLine(String line, int bytesRead, LineDecoder decoder) {
      // Everything after the response is complete is not ours to parse: a
      // bodyless response may be followed by bytes still in the decoder's
      // buffer, and feeding those through the header branch below would
      // fail as a malformed header line.
      if (finished) return;

      final normalizedLine = line.trimRight();
      if (inBody) {
        if (chunked) {
          if (expectingChunkSize) {
            final trimmed = line.trim();
            // Allow optional chunk extensions: <size>;(extension...)
            final semi = trimmed.indexOf(';');
            final sizeStr =
                (semi >= 0 ? trimmed.substring(0, semi) : trimmed).trim();
            currentChunkSize = int.parse(sizeStr, radix: 16);
            if (currentChunkSize == 0) {
              // Last-chunk; proceed to optional trailer headers terminated
              // by a blank line.
              expectingChunkSize = false;
              inTrailers = true;
              return;
            }
            // Read exactly currentChunkSize bytes for chunk data.
            expectingChunkSize = false;
            expectingChunkData = true;
            decoder.expectedByteCount = currentChunkSize;
            return;
          }
          if (expectingChunkData) {
            // Append chunk data (decoded as UTF-8 text).
            body.write(line);
            expectingChunkData = false;
            expectingChunkCRLF = true;
            // The next line in the stream is the trailing CRLF/LF after chunk data.
            // Leave decoder.expectedByteCount as -1 to scan until the next '\n'.
            return;
          }
          if (expectingChunkCRLF) {
            // Ignore CRLF and expect next chunk size line.
            expectingChunkCRLF = false;
            expectingChunkSize = true;
            return;
          }
          if (inTrailers) {
            // Read trailers until blank line, then finish.
            if (line.trim().isEmpty) {
              finished = true;
            } else {
              // Store trailer headers if needed.
              final separator = line.indexOf(':');
              if (separator > 0) {
                final name = line.substring(0, separator).toLowerCase().trim();
                final value = line.substring(separator + 1).trim();
                headers.putIfAbsent(name, () => []).add(value);
              }
            }
            return;
          }
          // Should not reach here under normal chunked flow.
          return;
        } else {
          body.write(line);
          contentRead += bytesRead;
        }
      } else if (inHeader) {
        if (normalizedLine.trim().isEmpty) {
          inBody = true;

          // RFC 9110 §6.4.1 and RFC 9112 §6.3: a 1xx, 204 or 304
          // response, and any response to HEAD, never has a body, whatever
          // the framing headers say. Stopping here matters because without
          // a `Content-Length` the read below runs until the peer closes
          // the connection -- for these responses that is a wait for
          // something that is never coming.
          if (_responseHasNoBody(statusCode, method)) {
            finished = true;
            return;
          }

          // Decide body framing.
          final te = headers[SSHHttpHeaders.transferEncodingHeader]
              ?.join(',')
              .toLowerCase();
          if (te != null && te.contains('chunked')) {
            chunked = true;
            expectingChunkSize = true;
          } else {
            // Identity transfer; use Content-Length when provided.
            if (contentLength > 0) {
              decoder.expectedByteCount = contentLength;
            }
          }
          return;
        }
        final separator = normalizedLine.indexOf(':');
        if (separator <= 0) {
          throw FormatException(
            'Invalid header line: "$normalizedLine" - no colon separator found',
          );
        }
        final name =
            normalizedLine.substring(0, separator).toLowerCase().trim();
        final value = normalizedLine.substring(separator + 1).trim();
        final normalizedValue = value.toLowerCase();
        if (name == SSHHttpHeaders.transferEncodingHeader &&
            normalizedValue != 'identity' &&
            normalizedValue != 'chunked') {
          throw UnsupportedError(
            "only 'identity' or 'chunked' transfer encodings are accepted: $normalizedValue",
          );
        }
        if (name == SSHHttpHeaders.contentLengthHeader) {
          contentLength = int.parse(value);
        }
        if (!headers.containsKey(name)) {
          headers[name] = [];
        }
        headers[name]!.add(value);
      } else if (normalizedLine.startsWith('HTTP/1.1') ||
          normalizedLine.startsWith('HTTP/1.0')) {
        statusCode = int.parse(
          normalizedLine.substring('HTTP/1.x '.length, 'HTTP/1.x xxx'.length),
        );
        reasonPhrase = normalizedLine.substring('HTTP/1.x xxx '.length);
        inHeader = true;
      } else {
        throw UnsupportedError('unsupported http response format');
      }
    }

    final lineDecoder = LineDecoder.withCallback(processLine);

    // An inactivity timeout rather than a deadline for the whole response,
    // so a large body that keeps arriving is never cut short. Applied to
    // the stream rather than around the loop so that firing it cancels the
    // subscription instead of leaving it running behind a failed future.
    final responseStream = idleTimeout == null
        ? socket.stream
        : socket.stream.timeout(idleTimeout);

    try {
      await for (final chunk in responseStream) {
        lineDecoder.add(chunk);
        if (finished) break;
        if (!chunked) {
          if (inHeader && inBody && contentLength >= 0) {
            if ((contentRead + lineDecoder.bufferedBytes) >= contentLength) {
              break;
            }
          }
        }
      }
      lineDecoder.close();
    } finally {
      // Also covers the paths that throw out of the loop -- a timeout, or a
      // malformed response -- which used to leave the socket open.
      socket.close();
    }

    // try {
    //   while (!inHeader ||
    //       !inBody ||
    //       ((contentRead + lineDecoder.bufferedBytes) < contentLength)) {
    //     final bytes = socket.readSync(1024);

    //     if (bytes == null || bytes.isEmpty) {
    //       break;
    //     }
    //     lineDecoder.add(bytes);
    //   }
    // } finally {
    //   try {
    //     lineDecoder.close();
    //   } finally {
    //     socket.closeSync();
    //   }
    // }

    return SSHHttpClientResponse._(
      headers,
      reasonPhrase: reasonPhrase,
      statusCode: statusCode,
      body: body.toString(),
    );
  }
}

/// Whether a response with this [statusCode], to a request with this
/// [method], is defined to carry no body at all.
///
/// Per RFC 9110 §6.4.1 that is any 1xx, 204 or 304 response, plus every
/// response to a HEAD request. For these the framing headers describe the
/// body the request would have had, so neither a `Content-Length` nor its
/// absence says anything about what is actually on the wire.
bool _responseHasNoBody(int? statusCode, String? method) {
  if (method != null && method.toUpperCase() == 'HEAD') return true;
  if (statusCode == null) return false;
  if (statusCode >= 100 && statusCode < 200) return true;
  return statusCode == 204 || statusCode == 304;
}

/// Parses a `Host` header value (e.g. `example.com`, `example.com:8080`,
/// or the IPv6 form `[::1]:22`) into a [Uri] whose `host`/`port` reflect
/// the authority.
///
/// [Uri.parse] treats a bare authority like `example.com:8080` as having
/// scheme `example.com` and path `8080` (host/port both empty/zero), so a
/// `//` prefix is added to make it unambiguous. A [val] that already
/// includes a scheme (e.g. `http://example.com`) is tolerated as-is.
Uri _parseHostHeaderUri(String val) {
  final hasScheme = RegExp(r'^[a-zA-Z][a-zA-Z0-9+\-.]*://').hasMatch(val);
  return Uri.parse(hasScheme ? val : '//$val');
}

class _SSHHttpClientResponseHeaders implements SSHHttpHeaders {
  final Map<String, List<String>> _headers;

  _SSHHttpClientResponseHeaders(this._headers);

  @override
  List<String>? operator [](String name) => _headers[name];

  @override
  void add(String name, Object value, {bool preserveHeaderCase = false}) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  bool get chunkedTransferEncoding =>
      value(SSHHttpHeaders.transferEncodingHeader)?.toLowerCase() == 'chunked';

  @override
  set chunkedTransferEncoding(bool chunkedTransferEncoding) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  int get contentLength {
    final val = value(SSHHttpHeaders.contentLengthHeader);
    if (val != null) {
      final parsed = int.tryParse(val);
      if (parsed != null) {
        return parsed;
      }
    }
    return -1;
  }

  @override
  set contentLength(int contentLength) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  SSHContentType? get contentType {
    final val = value(SSHHttpHeaders.contentTypeHeader);
    if (val != null) {
      return SSHContentType.parse(val);
    }
    return null;
  }

  @override
  set contentType(SSHContentType? contentType) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  set date(DateTime? date) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  DateTime? get date {
    final val = value(SSHHttpHeaders.dateHeader);
    if (val != null) {
      return parseHttpDate(val);
    }
    return null;
  }

  @override
  set expires(DateTime? expires) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  DateTime? get expires {
    final val = value(SSHHttpHeaders.expiresHeader);
    if (val != null) {
      return parseHttpDate(val);
    }
    return null;
  }

  @override
  void forEach(void Function(String name, List<String> values) f) =>
      _headers.forEach(f);

  @override
  set host(String? host) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  String? get host {
    final val = value(SSHHttpHeaders.hostHeader);
    if (val != null) {
      return _parseHostHeaderUri(val).host;
    }
    return null;
  }

  @override
  DateTime? get ifModifiedSince {
    final val = value(SSHHttpHeaders.ifModifiedSinceHeader);
    if (val != null) {
      return parseHttpDate(val);
    }
    return null;
  }

  @override
  set ifModifiedSince(DateTime? ifModifiedSince) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  void noFolding(String name) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  bool get persistentConnection => false;

  @override
  set persistentConnection(bool persistentConnection) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  set port(int? port) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  int? get port {
    final val = value(SSHHttpHeaders.hostHeader);
    if (val != null) {
      final uri = _parseHostHeaderUri(val);
      return uri.hasPort ? uri.port : null;
    }
    return null;
  }

  @override
  void remove(String name, Object value) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  void removeAll(String name) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  void set(String name, Object value, {bool preserveHeaderCase = false}) {
    throw UnsupportedError('Response headers are immutable');
  }

  @override
  String? value(String name) {
    final val = this[name];
    if (val == null || val.isEmpty) {
      return null;
    } else if (val.length == 1) {
      return val[0];
    } else {
      throw SSHHttpException('header $name has more than one value');
    }
  }

  @override
  void clear() {
    throw UnsupportedError('Response headers are immutable');
  }
}
