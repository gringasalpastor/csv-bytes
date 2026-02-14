#[derive(Clone, Copy, Debug, Default)]
pub enum Terminator {
    /// Parses `\r`, `\n` or `\r\n` as a single record terminator.
    #[default]
    CRLF,
    /// Parses the byte given as a record terminator.
    #[allow(dead_code)]
    Any(u8),
}

impl Terminator {
    /// Checks whether the terminator is set to CRLF.
    fn is_crlf(&self) -> bool {
        match *self {
            Terminator::CRLF => true,
            Terminator::Any(_) => false,
        }
    }

    fn equals(&self, other: u8) -> bool {
        match *self {
            Terminator::CRLF => other == b'\r' || other == b'\n',
            Terminator::Any(b) => other == b,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Offset(pub(crate) usize);

#[derive(Default)]
pub struct Nfa {
    config: ParserConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NfaAction {
    // Do not consume an input byte
    Epsilon,
    // Copy input byte to a caller-provided output buffer
    CopyToOutput,
    // Consume but do not copy input byte (for example, seeing a field
    // delimiter will consume an input byte but should not copy it to the
    // output buffer.
    Discard,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ScanAction {
    ScanUnquoted, // scan for: quote, delimiter, or term
    ScanQuoted,   // scan for: quote or escape
    AdvanceOne,   // No scan, advance 1
    NoScan,       // No scan, advance 0 (NfaAction::Epsilon | EndRecord)
    ScanComment,  // scan for newline
}

impl Nfa {
    fn transition_nfa(&self, state: NfaState, c: u8) -> (NfaState, NfaAction, ScanAction) {
        use self::NfaAction::*;
        use self::NfaState::*;
        use self::ScanAction::*;
        match state {
            StartRecord => {
                if self.config.term.equals(c) {
                    (StartRecord, Discard, ScanUnquoted)
                } else if self.config.comment == Some(c) {
                    (InComment, Discard, ScanComment)
                } else {
                    (StartField, Epsilon, NoScan)
                }
            }
            EndRecord => (StartRecord, Epsilon, NoScan),
            StartField => {
                if self.config.quoting && self.config.quote == c {
                    (InQuotedField, Discard, ScanQuoted)
                } else if self.config.delimiter == c {
                    (EndFieldDelim, Discard, ScanUnquoted)
                } else if self.config.term.equals(c) {
                    (EndFieldTerm, Epsilon, NoScan)
                } else {
                    (InField, CopyToOutput, ScanUnquoted)
                }
            }
            EndFieldDelim => (StartField, Epsilon, NoScan),
            EndFieldTerm => (InRecordTerm, Epsilon, NoScan),
            InField => {
                if self.config.delimiter == c {
                    (EndFieldDelim, Discard, ScanUnquoted)
                } else if self.config.term.equals(c) {
                    (EndFieldTerm, Epsilon, NoScan)
                } else {
                    (InField, CopyToOutput, ScanUnquoted)
                }
            }
            InQuotedField => {
                if self.config.quoting && self.config.quote == c {
                    (InDoubleEscapedQuote, Discard, AdvanceOne)
                } else if self.config.quoting && self.config.escape == Some(c) {
                    (InEscapedQuote, Discard, ScanQuoted)
                } else {
                    (InQuotedField, CopyToOutput, ScanQuoted)
                }
            }
            InEscapedQuote => (InQuotedField, CopyToOutput, ScanQuoted),
            InDoubleEscapedQuote => {
                if self.config.quoting && self.config.double_quote && self.config.quote == c {
                    (InQuotedField, CopyToOutput, ScanQuoted)
                } else if self.config.delimiter == c {
                    (EndFieldDelim, Discard, ScanUnquoted)
                } else if self.config.term.equals(c) {
                    (EndFieldTerm, Epsilon, NoScan)
                } else {
                    (InField, CopyToOutput, ScanUnquoted)
                }
            }
            InComment => {
                if b'\n' == c {
                    (StartRecord, Discard, ScanUnquoted)
                } else {
                    (InComment, Discard, ScanComment)
                }
            }
            InRecordTerm => {
                if self.config.term.is_crlf() && b'\r' == c {
                    (CRLF, Discard, AdvanceOne)
                } else {
                    (EndRecord, Discard, NoScan)
                }
            }
            CRLF => {
                if b'\n' == c {
                    (StartRecord, Discard, NoScan)
                } else {
                    (StartRecord, Epsilon, NoScan)
                }
            }
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum NfaState {
    // These states aren't used in the DFA, so we
    // assign them meaningless numbers.
    EndFieldTerm = 200,
    InRecordTerm = 201,

    // All states below are DFA states.
    StartRecord = 0,
    StartField = 1,
    InField = 2,
    InQuotedField = 3,
    InEscapedQuote = 4,
    InDoubleEscapedQuote = 5,
    InComment = 6,
    // All states below are "final field" states.
    // Namely, they indicate that a field has been parsed.
    EndFieldDelim = 7,
    // All states below are "final record" states.
    // Namely, they indicate that a record has been parsed.
    EndRecord = 8,
    CRLF = 9,
}
impl NfaState {
    /// Returns true if this state indicates that a field has been parsed.
    fn is_field_final(&self) -> bool {
        matches!(
            *self,
            NfaState::EndRecord | NfaState::CRLF | NfaState::EndFieldDelim
        )
    }

    // /// Returns true if this state indicates that a record has been parsed.
    // fn is_record_final(&self) -> bool {
    //     matches!(*self, NfaState::EndRecord | NfaState::CRLF)
    // }
}

#[derive(Clone, Debug)]
pub struct ParserConfig {
    delimiter: u8,
    term: Terminator,
    quote: u8,
    escape: Option<u8>,
    double_quote: bool,
    comment: Option<u8>,
    quoting: bool,
    num_fields: u32,
    find_a_record_end_max_tries: u32,
    find_a_record_end_required_good_scans: u32,
}

impl Default for ParserConfig {
    fn default() -> ParserConfig {
        ParserConfig {
            delimiter: b',',
            term: Terminator::default(),
            quote: b'"',
            escape: None,
            double_quote: true,
            comment: None,
            quoting: true,
            num_fields: 8,
            find_a_record_end_max_tries: 5,
            find_a_record_end_required_good_scans: 3,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum ScanRecordError {
    WrongNumOfFields(Offset),
    InputExhausted,
}

#[derive(Copy, Clone, Debug)]
pub enum FindARecordEndError {
    InputExhausted,
    ExceededMaxTries,
}
pub trait FiniteAutomata {
    fn scan_record(&self, input: &[u8]) -> Result<Offset, ScanRecordError>;
    fn find_a_record_end(&self, buffer: &[u8]) -> Result<Offset, FindARecordEndError>;
}

#[inline(always)]
fn scan(scan_action: &ScanAction, buffer: &[u8]) -> Option<Offset> {
    match scan_action {
        ScanAction::AdvanceOne => Some(Offset(1)),
        ScanAction::NoScan => Some(Offset(0)),
        ScanAction::ScanComment => {
            let slice = buffer.get(1..).unwrap_or(&[]);
            Some(Offset(memchr::memchr(b'\n', slice)? + 1))
        }
        ScanAction::ScanQuoted => {
            let slice = buffer.get(1..).unwrap_or(&[]);
            Some(Offset(memchr::memchr2(b'"', b'\\', slice)? + 1))
        }
        ScanAction::ScanUnquoted => {
            let slice = buffer.get(1..).unwrap_or(&[]);
            Some(Offset(memchr::memchr3(b'"', b',', b'\n', slice)? + 1))
        }
    }
}

impl FiniteAutomata for Nfa {
    #[inline(always)]
    fn scan_record(&self, input: &[u8]) -> Result<Offset, ScanRecordError> {
        let mut pos = 0;
        let mut state = NfaState::StartRecord;
        let mut fields_count: u32 = 0;
        while pos < input.len() {
            let (s, _nfa_action, scan_action) = self.transition_nfa(state, input[pos]);
            match scan(&scan_action, &input[pos..]) {
                Some(offset) => pos += offset.0,
                None => {
                    return Err(ScanRecordError::InputExhausted);
                }
            }
            state = s;

            // println!("state: {state:?}, scan_action: {:?}", &scan_action);
            // println!("{:?}", str::from_utf8(&input[0..pos]));

            if state.is_field_final() {
                fields_count += 1;
                if fields_count > self.config.num_fields {
                    return Err(ScanRecordError::WrongNumOfFields(Offset(pos)));
                }
                if state != NfaState::EndFieldDelim {
                    if fields_count < self.config.num_fields {
                        return Err(ScanRecordError::WrongNumOfFields(Offset(pos)));
                    }
                    return Ok(Offset(pos));
                }
            }
        }

        Err(ScanRecordError::InputExhausted)
    }

    fn find_a_record_end(&self, buffer: &[u8]) -> Result<Offset, FindARecordEndError> {
        let mut cur_offset =
            memchr::memchr(b'\n', buffer).ok_or(FindARecordEndError::InputExhausted)?;

        let mut num_good_scans = 0;
        let mut try_count = 0;

        while num_good_scans < self.config.find_a_record_end_required_good_scans {
            if try_count >= self.config.find_a_record_end_max_tries {
                return Err(FindARecordEndError::ExceededMaxTries);
            }
            let result = self.scan_record(&buffer[cur_offset..]);

            match result {
                Ok(bytes_consumed) => {
                    // println!(
                    //     "{:?}",
                    //     str::from_utf8(&buffer[cur_offset..=cur_offset + bytes_consumed.0])
                    // );
                    cur_offset += bytes_consumed.0;
                    num_good_scans += 1;
                }
                Err(ScanRecordError::WrongNumOfFields(bytes_consumed)) => {
                    cur_offset += bytes_consumed.0;
                    num_good_scans = 0;
                    try_count += 1;

                    // advance to the next terminator
                    let slice = buffer.get(cur_offset + 1..).unwrap_or(&[]);
                    cur_offset +=
                        memchr::memchr(b'\n', slice).ok_or(FindARecordEndError::InputExhausted)?;
                }
                Err(ScanRecordError::InputExhausted) => {
                    return Err(FindARecordEndError::InputExhausted);
                }
            }
        }

        Ok(Offset(cur_offset))
    }
}
