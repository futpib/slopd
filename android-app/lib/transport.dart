import 'dart:convert';
import 'dart:ffi';
import 'dart:isolate';

import 'package:ffi/ffi.dart';

Map<String, dynamic> _nativeRequest(String request) {
  final lib = DynamicLibrary.open('libslopd_android_transport.so');
  final call = lib
      .lookupFunction<
        Pointer<Utf8> Function(Pointer<Utf8>),
        Pointer<Utf8> Function(Pointer<Utf8>)
      >('slopd_iroh_request');
  final free = lib
      .lookupFunction<
        Void Function(Pointer<Utf8>),
        void Function(Pointer<Utf8>)
      >('slopd_iroh_free');
  final input = request.toNativeUtf8();
  try {
    final result = call(input);
    try {
      final response =
          jsonDecode(result.toDartString()) as Map<String, dynamic>;
      if (response['ok'] != true) throw StateError(response['error'] as String);
      return response['result'] as Map<String, dynamic>;
    } finally {
      free(result);
    }
  } finally {
    calloc.free(input);
  }
}

class IrohTransport {
  static Future<int> connect(String endpoint, List<String> relays) async {
    final request = jsonEncode({
      'method': 'connect',
      'endpoint': endpoint,
      'relays': relays,
    });
    final result = await Isolate.run(() => _nativeRequest(request));
    return result['port'] as int;
  }

  static Future<void> disconnect(int port) async {
    final request = jsonEncode({'method': 'disconnect', 'port': port});
    await Isolate.run(() => _nativeRequest(request));
  }
}
