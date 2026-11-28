package com.sidewire;

import java.lang.reflect.Method;

public final class ShellWakeLock {

    private ShellWakeLock() {}

    public static void main(String[] args) throws Exception {
        Class<?> looper = Class.forName("android.os.Looper");
        if (looper.getMethod("getMainLooper").invoke(null) == null) {
            looper.getMethod("prepareMainLooper").invoke(null);
        }
        Class<?> thread = Class.forName("android.app.ActivityThread");
        Object instance = thread.getMethod("systemMain").invoke(null);
        Object context = thread.getMethod("getSystemContext").invoke(instance);
        Object power = Class.forName("android.content.Context")
            .getMethod("getSystemService", String.class)
            .invoke(context, "power");
        Class<?> manager = Class.forName("android.os.PowerManager");
        Object lock = manager
            .getMethod("newWakeLock", int.class, String.class)
            .invoke(
                power,
                manager.getField("PARTIAL_WAKE_LOCK").getInt(null),
                "SideWire:interactive-shell"
            );
        Class<?> type = Class.forName("android.os.PowerManager$WakeLock");
        Method release = type.getMethod("release");
        type.getMethod("acquire").invoke(lock);
        try {
            // The daemon owns the only write end, including across abrupt disconnects.
            while (System.in.read() != -1) {}
        } finally {
            release.invoke(lock);
        }
        System.exit(0);
    }
}
