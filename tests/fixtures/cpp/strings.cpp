


#include <string>

__attribute__((noinline)) int strings_target(const std::string &short_text,
                                             const std::string &long_text) {
    std::string empty;
    std::string with_nul("a\0b", 3);
    wchar_t wide = L'\u00e9';
    char8_t eight = u8'a';
    char16_t sixteen = u'\u03bb';
    char32_t thirty_two = U'\U0001F980';
    char16_t surrogate = 0xd800;
    const char16_t *sixteen_text = u"\u03bbx";
    const char8_t *eight_text = u8"\u00e9ight";
    asm volatile("" : : "g"(&short_text), "g"(&long_text), "g"(&empty), "g"(&with_nul),
                 "g"(&wide), "g"(&eight), "g"(&sixteen), "g"(&thirty_two), "g"(&surrogate),
                 "g"(&sixteen_text), "g"(&eight_text)
                 : "memory");
    return static_cast<int>(short_text.size() + with_nul.size()); // strings stop here
}

int main() {
    std::string short_text = "short";
    std::string long_text(300, 'y');
    return strings_target(short_text, long_text) == 8 ? 0 : 1;
}
