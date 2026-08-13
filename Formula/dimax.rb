class Dimax < Formula
  desc "Terminal multiplexer with persistent sessions and synchronized workspaces"
  homepage "https://github.com/DIvkov575/dimax"
  head "https://github.com/DIvkov575/dimax.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      dimax does not modify terminal configuration during `brew install`.

      Portable keybindings are active by default. To install Kitty Cmd
      bindings, or enable both input modes, run:

        dimax keys install --mode kitty
        dimax keys install --mode both --reload

      Remove managed Kitty configuration with:

        dimax keys uninstall
    EOS
  end

  test do
    assert_match "Usage: dimax", shell_output("#{bin}/dimax --help")
    assert_match "Portable prefix: Ctrl-Space",
      shell_output("XDG_CONFIG_HOME=#{testpath}/config #{bin}/dimax keys print --mode portable")
  end
end
