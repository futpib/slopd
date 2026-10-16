package io.github.futpib.slopd_control

import android.content.Context
import android.os.Bundle
import io.flutter.embedding.android.FlutterActivity

class MainActivity : FlutterActivity() {
    private external fun initVerifier(context: Context)

    override fun onCreate(savedInstanceState: Bundle?) {
        System.loadLibrary("slopd_android_transport")
        initVerifier(applicationContext)
        super.onCreate(savedInstanceState)
    }

}
