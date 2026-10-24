cask "voiceislocal" do
  version "1.0.0"
  sha256 "8c200a04b0753dfa3b20409c450a152c9cdcc7fc74b3a69775e57af2551065c8"

  url "https://voiceislocal.com/downloads/VoiceIsLocal-#{version}.dmg"
  name "Voice is Local"
  desc "On-device dictation, meeting transcription and reading aloud"
  homepage "https://voiceislocal.com/"

  livecheck do
    url :homepage
    regex(/VoiceIsLocal[._-]v?(\d+(?:\.\d+)+)\.dmg/i)
  end

  depends_on arch: :arm64
  depends_on macos: :golden_gate

  app "VoiceIsLocal.app"
  binary "#{appdir}/VoiceIsLocal.app/Contents/MacOS/voiceislocal"

  zap trash: [
    "~/Library/Application Support/Holos",
    "~/Library/Preferences/ca.orlenko.holos.app.plist",
  ]
end
