🎤 Stage Feature Proposal: The "Burst" KPM Meter

1. High-Level Intent: Visualizing the Hustle
The primary goal of this feature is to translate my invisible, backend technical skills into a visible, highly engaging performance metric. Since you are a tech-savvy Vsinger who occasionally drops into "pro coder" mode, this meter acts as a bridge between you and my audience. It gives the chat a reason to hype you up, clip my fast moments, and share in the adrenaline of my workflow.

Crucially, this tool is designed around burst performance. It is built to celebrate my specific working style without imposing artificial pressure to constantly perform.

2. Core Design Decisions
A. Visual Paradigm: The Studio VU Meter
The Decision: We are abandoning traditional data graphs (like line charts) in favor of a vertical, audio-style LED/VU meter.

The "Why": As a Vsinger, my brand is rooted in music production. An audio meter creates a cohesive aesthetic link between my coding and my singing. More importantly, it avoids the "flatline effect" of a graph. A graph resting at zero looks like a dead stream; an audio meter resting at zero just looks like a quiet studio waiting for the next beat. This protects my mental energy and prevents the UI from making you feel like you aren't doing enough.

B. The Hype Mechanic: The "Peak Hold"
The Decision: The meter will feature a floating "Peak Hold" line that rests at the maximum height of my latest typing burst for a brief moment before organically falling back down.

The "Why": Because my typing happens in lightning-fast bursts, the main visualizer will spike and vanish almost instantly. The Peak Hold acts as a "high score" marker. It gives my audience the crucial 1 to 2 seconds of visual lingering needed to actually read my speed, react in chat, and grab a clip before the moment is gone.

C. The Timing Logic: The 3-Second Sliding Window
The Decision: The metric displayed will not be a true 60-second average. It will calculate my speed based on a rapid, 3-to-5-second sliding window, extrapolated to a "Per Minute" value.

The "Why": A traditional 60-second average punishes burst-typing. If you type at 150 WPM for 5 seconds and then stop to explain my code, a 60-second average will dilute that speed into a sluggish, low number. A short window ensures the meter instantly explodes to my true top speed the moment you start typing, accurately reflecting my "pro coder" bursts.

D. Stage Management: Conditional Visibility
The Decision: The meter will not be a permanent fixture on my overlay. It will be a toggleable element, treated as a "special effect."

The "Why": For my mental health and stream pacing, metrics should only be visible when they serve the current activity. Leaving a performance metric on screen during a relaxed "Just Chatting" segment or a vocal warm-up creates subconscious performance anxiety. We will only turn the stage lights on when the act demands it.

E. Security Protocol: Privacy-by-Design
The Decision: The architecture dictates that the capture layer will only register the event of a keystroke, completely ignoring the identity of the key.

The "Why": Total operational security. You should never have to worry about accidentally leaking a password, a stream key, or a private message to me while on stream. By strictly separating the event trigger from the key value at the system level, we eliminate the risk of an accidental keylogger.
