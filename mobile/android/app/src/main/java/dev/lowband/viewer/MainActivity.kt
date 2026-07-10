package dev.lowband.viewer

import android.os.Bundle
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity

/**
 * LowBand Viewer — assisted-side mobile client (pre-flight preview).
 *
 * v0.1 scope: join-code entry and session state display. The LBTP session
 * itself runs in the Rust core (`core/lbtp`), which this app will link over
 * FFI in a later milestone; until then joining validates the code format and
 * shows the consent-first session screen with no live transport.
 */
class MainActivity : AppCompatActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        val codeInput = findViewById<EditText>(R.id.join_code)
        val joinButton = findViewById<Button>(R.id.join_button)
        val status = findViewById<TextView>(R.id.status)

        joinButton.setOnClickListener {
            val code = codeInput.text.toString().trim().uppercase()
            if (!JOIN_CODE.matches(code)) {
                status.text = getString(R.string.status_bad_code)
                return@setOnClickListener
            }
            // Transport lands with the FFI milestone; make the preview state
            // unmistakable so testers never mistake this for a live session.
            status.text = getString(R.string.status_preview, code)
        }
    }

    private companion object {
        /** Short join codes as issued by lowband-signaling, e.g. K7F-2QX. */
        val JOIN_CODE = Regex("^[A-Z0-9]{3}-[A-Z0-9]{3}$|^[0-9]{9}$")
    }
}
