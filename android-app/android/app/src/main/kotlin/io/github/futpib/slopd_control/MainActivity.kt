package io.github.futpib.slopd_control

import android.content.Context
import android.content.Intent
import android.os.Bundle
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine

class MainActivity : FlutterActivity() {
    private external fun initVerifier(context: Context)

    override fun onCreate(savedInstanceState: Bundle?) {
        System.loadLibrary("slopd_android_transport")
        initVerifier(applicationContext)
        super.onCreate(savedInstanceState)
    }

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        OpenKeychainBridge.register(flutterEngine.dartExecutor.binaryMessenger, this)
    }

    override fun onResume() {
        super.onResume()
        OpenKeychainBridge.attachActivity(this)
    }

    override fun onPause() {
        OpenKeychainBridge.detachActivity(this)
        super.onPause()
    }

    @Deprecated("Deprecated in Android")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        OpenKeychainBridge.attachActivity(this)
        if (!OpenKeychainBridge.handleActivityResult(requestCode, resultCode, data)) {
            super.onActivityResult(requestCode, resultCode, data)
        }
    }
}
